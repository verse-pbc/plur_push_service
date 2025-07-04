use crate::{
    // config::CONFIG,
    error::Result,
    // fcm::{send_fcm_message, FcmPayload},
    fcm_sender,
    models::{FcmNotification, FcmPayload},
    nostr::nip29,
    redis_store,
    state::AppState,
};
use nostr_sdk::prelude::*;
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, trace, warn};

const KIND_REGISTRATION: Kind = Kind::Custom(3079);
const KIND_DEREGISTRATION: Kind = Kind::Custom(3080);
const BROADCASTABLE_EVENT_KINDS: [Kind; 2] = [Kind::Custom(11), Kind::Custom(12)];
// Add other kinds like Kind::TextNote, Kind::Reaction etc.
const KIND_GROUP_MESSAGE: Kind = Kind::Custom(11); // Example, adjust as needed
const KIND_GROUP_REPLY: Kind = Kind::Custom(12); // Example, adjust as needed

pub async fn run(
    state: Arc<AppState>,
    mut event_rx: Receiver<Box<Event>>,
    token: CancellationToken,
) -> Result<()> {
    tracing::info!("Starting event handler...");

    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                info!("Event handler cancellation received. Shutting down...");
                break;
            }

            maybe_event = event_rx.recv() => {
                let Some(event) = maybe_event else {
                    info!("Event channel closed. Event handler shutting down.");
                    break;
                };

                let event_id = event.id;
                let event_kind = event.kind;
                let pubkey = event.pubkey;

                debug!(event_id = %event_id, kind = %event_kind, pubkey = %pubkey, "Event handler received event");

                tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        info!("Event handler cancelled while checking if event {} was processed.", event_id);
                        break;
                    }
                    processed_result = redis_store::is_event_processed(&state.redis_pool, &event_id) => {
                        match processed_result {
                            Ok(true) => {
                                trace!(event_id = %event_id, "Skipping already processed event");
                                continue;
                            }
                            Ok(false) => {
                                // Not processed, continue handling
                            }
                            Err(e) => {
                                error!(event_id = %event_id, error = %e, "Failed to check if event was processed");
                                continue;
                            }
                        }
                    }
                }

                debug!(event_id = %event_id, kind = %event_kind, "Dispatching event handler");

                let handler_result = if event_kind == KIND_REGISTRATION {
                    handle_registration(&state, &event).await
                } else if event_kind == KIND_DEREGISTRATION {
                    handle_deregistration(&state, &event).await
                } else if event_kind == KIND_GROUP_MESSAGE || event_kind == KIND_GROUP_REPLY {
                    handle_group_message(&state, &event, token.clone()).await
                } else {
                    warn!(event_id = %event_id, kind = %event_kind, "Ignoring event with unhandled kind");
                    Ok(())
                };

                match handler_result {
                    Ok(_) => {
                        trace!(event_id = %event_id, kind = %event_kind, "Handler finished successfully");
                        tokio::select! {
                            biased;
                            _ = token.cancelled() => {
                                info!("Event handler cancelled before marking event {} as processed.", event_id);
                                break;
                            }
                            mark_result = redis_store::mark_event_processed(
                                &state.redis_pool,
                                &event_id,
                                state.settings.service.processed_event_ttl_secs,
                            ) => {
                                if let Err(e) = mark_result {
                                    error!(event_id = %event_id, error = %e, "Failed to mark event as processed");
                                } else {
                                    debug!(event_id = %event_id, "Successfully marked event as processed");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Removed downcast_ref check for ServiceError::Cancelled
                        // Cancellation is handled by the select! blocks
                        /*
                        if let Some(service_error) = e.downcast_ref::<crate::error::ServiceError>() {
                            if matches!(service_error, crate::error::ServiceError::Cancelled) {
                                info!(event_id = %event_id, "Handler for event cancelled internally.");
                                break; // Exit outer loop if handler was cancelled
                            }
                        }
                        */
                        error!(event_id = %event_id, error = %e, "Failed to handle event");
                        // Decide if the error is fatal or if we should continue processing other events
                        // For now, continue processing other events
                    }
                }

                if token.is_cancelled() {
                    info!(event_id = %event_id, "Event handler cancellation detected after processing event {}.", event_id);
                    break;
                }
            }
        }
    }

    info!("Event handler shut down.");
    Ok(())
}

async fn handle_registration(state: &AppState, event: &Event) -> Result<()> {
    assert!(event.kind == KIND_REGISTRATION);

    let fcm_token = event.content.trim();
    if fcm_token.is_empty() {
        warn!(
            event_id = %event.id, pubkey = %event.pubkey,
            "Received registration event with empty token"
        );
        return Ok(());
    }

    redis_store::add_or_update_token(&state.redis_pool, &event.pubkey, fcm_token).await?;
    info!(event_id = %event.id, pubkey = %event.pubkey, "Registered/Updated token");
    Ok(())
}

async fn handle_deregistration(state: &AppState, event: &Event) -> Result<()> {
    assert!(event.kind == KIND_DEREGISTRATION);

    let fcm_token = event.content.trim();
    if fcm_token.is_empty() {
        warn!(
            event_id = %event.id, pubkey = %event.pubkey,
            "Received deregistration event with empty token"
        );
        return Ok(());
    }

    let removed = redis_store::remove_token(&state.redis_pool, &event.pubkey, fcm_token).await?;
    if removed {
        info!(event_id = %event.id, pubkey = %event.pubkey, "Deregistered token");
    } else {
        debug!(
            event_id = %event.id, pubkey = %event.pubkey, token_prefix = &fcm_token[..8.min(fcm_token.len())],
            "Token not found for deregistration"
        );
    }
    Ok(())
}

async fn handle_group_message(
    state: &AppState,
    event: &Event,
    token: CancellationToken,
) -> Result<()> {
    debug!(event_id = %event.id, kind = %event.kind, "Handling group message/reply");

    if token.is_cancelled() {
        info!(event_id = %event.id, "Cancelled before handling group message.");
        return Err(crate::error::ServiceError::Cancelled);
    }

    // Extract the group ID from the first 'h' tag found
    let group_id = event.tags.find(TagKind::h()).and_then(|tag| tag.content());

    let Some(group_id) = group_id else {
        warn!(event_id = %event.id, "Group message event missing 'h' tag or group ID, cannot determine group. Skipping.");
        return Ok(());
    };
    debug!(event_id = %event.id, %group_id, "Extracted group ID");

    // Check if this is a broadcast message (has a 'broadcast' tag)
    let is_broadcast = event.tags.find(TagKind::custom("broadcast")).is_some();

    if is_broadcast {
        debug!(event_id = %event.id, %group_id, "Broadcast tag detected, will notify all group members");
        return handle_broadcast_message(state, event, group_id, token).await;
    }

    // Continue with regular mention-based notification logic
    let mentioned_pubkeys = extract_mentioned_pubkeys(event);
    debug!(event_id = %event.id, mentions = mentioned_pubkeys.len(), "Extracted mentioned pubkeys");

    if mentioned_pubkeys.is_empty() {
        debug!(event_id = %event.id, "No mentioned pubkeys found, skipping notification.");
        return Ok(());
    }

    for target_pubkey in mentioned_pubkeys {
        if token.is_cancelled() {
            info!(event_id = %event.id, "Cancelled during group message pubkey processing loop.");
            return Err(crate::error::ServiceError::Cancelled);
        }

        trace!(event_id = %event.id, target_pubkey = %target_pubkey, %group_id, "Processing mention for group");

        if target_pubkey == event.pubkey {
            trace!(event_id = %event.id, target_pubkey = %target_pubkey, "Skipping notification to self");
            continue;
        }

        // Check if the mentioned user is a member of the group
        match state
            .nip29_client
            .is_group_member(&group_id, &target_pubkey)
            .await
        {
            Ok(true) => {
                trace!(event_id = %event.id, target_pubkey = %target_pubkey, %group_id, "Target user is a group member. Proceeding with notification.");
                // User is a member, continue to send notification
            }
            Ok(false) => {
                debug!(event_id = %event.id, target_pubkey = %target_pubkey, %group_id, "Target user is not a member of the group. Skipping notification.");
                continue; // Skip this user, move to the next mentioned pubkey
            }
            Err(e) => {
                error!(event_id = %event.id, target_pubkey = %target_pubkey, %group_id, error = %e, "Failed to check group membership. Skipping notification for this user.");
                continue; // Skip this user due to error, move to the next
            }
        }

        if let Err(e) = send_notification_to_user(state, event, &target_pubkey, token.clone()).await
        {
            if matches!(e, crate::error::ServiceError::Cancelled) {
                return Err(e);
            }
            error!(event_id = %event.id, target_pubkey = %target_pubkey, error = %e, "Failed to send notification to user");
        }
    }

    debug!(event_id = %event.id, "Finished handling group message/reply");
    Ok(())
}

/// Handle a broadcast message by sending notifications to all group members.
#[instrument(skip_all, fields(%group_id))]
async fn handle_broadcast_message(
    state: &AppState,
    event: &Event,
    group_id: &str,
    token: CancellationToken,
) -> Result<()> {
    info!(event_id = %event.id, %group_id, "Processing broadcast message");

    let kind_allowed = BROADCASTABLE_EVENT_KINDS.contains(&event.kind);
    if !kind_allowed {
        warn!(
            event_id = %event.id,
            kind = %event.kind,
            "Event kind is not allowed for broadcast. Skipping.",
        );
        return Ok(());
    }
    debug!(event_id = %event.id, kind = %event.kind, "Event kind is allowed for broadcast");

    let admin_check_result = state
        .nip29_client
        .is_group_admin(group_id, &event.pubkey)
        .await;

    match admin_check_result {
        Ok(true) => {
            debug!(event_id = %event.id, pubkey = %event.pubkey, "Sender IS an admin. Proceeding with broadcast.");
        }
        Ok(false) => {
            warn!(event_id = %event.id, pubkey = %event.pubkey, "Sender is NOT an admin. Rejecting broadcast attempt.");
            return Ok(()); // Not an error, just reject the unauthorized broadcast
        }
        Err(e) => {
            error!(event_id = %event.id, pubkey = %event.pubkey, error = %e, "Failed to check admin status for broadcast. Skipping.");
            return Err(e.into());
        }
    }

    let members_result = state.nip29_client.get_group_members(group_id).await;

    let members = match members_result {
        Ok(members) => {
            info!(event_id = %event.id, %group_id, count = members.len(), "Retrieved group members for broadcast");
            members
        }
        Err(e) => {
            error!(event_id = %event.id, %group_id, error = %e, "Failed to get group members for broadcast");
            return Err(e);
        }
    };

    info!(
        event_id = %event.id, %group_id,
        "Proceeding to send notifications to {} members (excluding sender)",
        members.len().saturating_sub(1)
    );

    let sender_pubkey = event.pubkey;
    let mut success_count = 0;
    let mut error_count = 0;
    let member_count = members.len();

    let members_vec: Vec<_> = members.into_iter().collect();
    for member_pubkey in members_vec {
        if token.is_cancelled() {
            info!(event_id = %event.id, %group_id, "Cancelled during broadcast processing");
            return Err(crate::error::ServiceError::Cancelled);
        }

        if member_pubkey == sender_pubkey {
            trace!(event_id = %event.id, target_pubkey = %member_pubkey, "Skipping notification to sender");
            continue;
        }

        match send_notification_to_user(state, event, &member_pubkey, token.clone()).await {
            Ok(_) => {
                success_count += 1;
            }
            Err(e) => {
                if matches!(e, crate::error::ServiceError::Cancelled) {
                    return Err(e);
                }
                error!(event_id = %event.id, target_pubkey = %member_pubkey, error = %e, "Failed to send broadcast notification to user");
                error_count += 1;
            }
        }
    }

    info!(
        event_id = %event.id, %group_id,
        total = member_count - 1, // Subtract 1 to account for sender
        success = success_count,
        errors = error_count,
        "Broadcast notification completed"
    );

    Ok(())
}

/// Send a notification to a specific user
#[instrument(skip_all, fields(target_pubkey = %target_pubkey.to_string()))]
async fn send_notification_to_user(
    state: &AppState,
    event: &Event,
    target_pubkey: &PublicKey,
    token: CancellationToken,
) -> Result<()> {
    let event_id = event.id;

    let tokens = tokio::select! {
        biased;
        _ = token.cancelled() => {
            info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled while fetching tokens.");
            return Err(crate::error::ServiceError::Cancelled);
        }
        res = redis_store::get_tokens_for_pubkey(&state.redis_pool, target_pubkey) => {
            res?
        }
    };
    trace!(event_id = %event_id, target_pubkey = %target_pubkey, count = tokens.len(), "Found tokens");

    if tokens.is_empty() {
        trace!(event_id = %event_id, target_pubkey = %target_pubkey, "No registered tokens found, skipping.");
        return Ok(());
    }

    trace!(event_id = %event_id, target_pubkey = %target_pubkey, "Creating FCM payload");
    let payload = create_fcm_payload(event)?;

    trace!(event_id = %event_id, target_pubkey = %target_pubkey, token_count = tokens.len(), "Attempting to send FCM notification");

    let results = tokio::select! {
        biased;
        _ = token.cancelled() => {
            info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled during FCM send batch.");
            return Err(crate::error::ServiceError::Cancelled);
        }
        send_result = state.fcm_client.send_batch(&tokens, payload) => {
            send_result
        }
    };
    trace!(event_id = %event_id, target_pubkey = %target_pubkey, results_count = results.len(), "FCM send completed");

    trace!(event_id = %event_id, target_pubkey = %target_pubkey, "Handling FCM results");
    let mut tokens_to_remove = Vec::new();
    let mut success_count = 0;
    for (fcm_token, result) in results {
        if token.is_cancelled() {
            info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled while processing FCM results.");
            return Err(crate::error::ServiceError::Cancelled);
        }

        let truncated_token = &fcm_token[..8.min(fcm_token.len())];

        match result {
            Ok(_) => {
                success_count += 1;
                trace!(target_pubkey = %target_pubkey, token_prefix = truncated_token, "Successfully sent notification");
            }
            Err(fcm_sender::FcmError::TokenNotRegistered) => {
                warn!(target_pubkey = %target_pubkey, token_prefix = truncated_token, "Token invalid/unregistered, marking for removal.");
                tokens_to_remove.push(fcm_token);
            }
            Err(e) => {
                error!(
                    target_pubkey = %target_pubkey, token_prefix = truncated_token, error = %e, error_debug = ?e,
                    "FCM send failed for token"
                );
            }
        }
    }
    debug!(event_id = %event_id, target_pubkey = %target_pubkey, success = success_count, removed = tokens_to_remove.len(), "FCM send summary");

    if !tokens_to_remove.is_empty() {
        debug!(event_id = %event_id, target_pubkey = %target_pubkey, count = tokens_to_remove.len(), "Removing invalid tokens globally");
        for fcm_token_to_remove in tokens_to_remove {
            if token.is_cancelled() {
                info!(event_id = %event_id, target_pubkey = %target_pubkey, "Cancelled while removing invalid tokens.");
                return Err(crate::error::ServiceError::Cancelled);
            }
            let truncated_token = &fcm_token_to_remove[..8.min(fcm_token_to_remove.len())];
            if let Err(e) = redis_store::remove_token_globally(
                &state.redis_pool,
                target_pubkey,
                &fcm_token_to_remove,
            )
            .await
            {
                error!(
                    target_pubkey = %target_pubkey, token_prefix = truncated_token, error = %e,
                    "Failed to remove invalid token globally"
                );
            } else {
                info!(target_pubkey = %target_pubkey, token_prefix = truncated_token, "Removed invalid token globally");
            }
        }
    } else {
        trace!(event_id = %event_id, target_pubkey = %target_pubkey, "No invalid tokens to remove");
    }
    trace!(event_id = %event_id, target_pubkey = %target_pubkey, "Finished sending notification");

    Ok(())
}

// Extracts pubkeys mentioned in any tag starting with "p".
// Delegates to the helper in the nip29 module.
fn extract_mentioned_pubkeys(event: &Event) -> Vec<nostr_sdk::PublicKey> {
    nip29::extract_pubkeys_from_p_tags(&event.tags).collect()
}

fn create_fcm_payload(event: &Event) -> Result<FcmPayload> {
    let title = format!(
        "New message from {}",
        &event
            .pubkey
            .to_bech32()
            .unwrap()
            .chars()
            .take(12)
            .collect::<String>()
    );
    let body = event.content.chars().take(150).collect();

    let mut data = std::collections::HashMap::new();
    data.insert("nostrEventId".to_string(), event.id.to_hex());

    // Extract group ID from 'h' tag using find() and content()
    let group_id = event
        .tags
        .find(TagKind::h()) // Find the first raw Tag with kind 'h'
        .and_then(|tag| tag.content()); // Get the content (value at index 1)

    if let Some(id_str) = group_id {
        // id_str is Option<&str>, convert to String for insertion
        data.insert("groupId".to_string(), id_str.to_string());
    }

    // Add other relevant event details here if needed by the client app.
    // These will be sent in the top-level `data` field of the FCM message.

    Ok(FcmPayload {
        // Basic cross-platform notification fields.
        // These are mapped to `firebase_messaging_rs::fcm::Notification`.
        notification: Some(FcmNotification {
            title: Some(title),
            body: Some(body),
            // Note: FcmNotification in models.rs only has title and body.
            // Other common fields like icon, sound, etc., are not defined there.
        }),
        // Arbitrary key-value data for the client application.
        // This is mapped to the `data` field in `firebase_messaging_rs::fcm::Message::Token`.
        data: Some(data),
        // Platform-specific configurations (currently NOT mapped/used by fcm_sender.rs).
        // These fields exist in FcmPayload (as defined in models.rs) to align
        // with the potential structure of the FCM v1 API message object, but the
        // current implementation in fcm_sender.rs only uses .notification and .data.
        // To use these, the mapping logic in fcm_sender.rs would need enhancement.
        android: None, // Example: Populate with serde_json::Value if needed.
        webpush: None, // Example: Populate with serde_json::Value if needed.
        apns: None,    // Example: Populate with serde_json::Value if needed.
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use nostr_sdk::prelude::{EventBuilder, Keys, Kind, SecretKey, Tag, Timestamp};

    #[tokio::test]
    async fn test_create_fcm_payload_full() {
        // Use a fixed secret key for deterministic pubkey and event ID
        let sk =
            SecretKey::from_hex("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let keys = Keys::new(sk);

        let content = "This is a test group message mentioning someone. It has more than 150 characters to ensure that the truncation logic is tested properly. Let's add even more text here to be absolutely sure that it exceeds the limit significantly.";

        let kind = Kind::Custom(11);
        let fixed_timestamp = Timestamp::from(0); // Use fixed timestamp for deterministic event ID
        let group_id = "test_group_id"; // Define a group ID for the test

        // Create the ["h", <group_id>] tag correctly
        let h_tag =
            Tag::parse(["h".to_string(), group_id.to_string()]).expect("Failed to parse h tag");

        let test_event = EventBuilder::new(kind, content)
            .tag(h_tag) // Use .tag()
            .custom_created_at(fixed_timestamp)
            .sign(&keys)
            .await
            .unwrap();

        // Get the actual event ID after signing
        let actual_event_id = test_event.id.to_hex();

        let payload_result = create_fcm_payload(&test_event);
        assert!(payload_result.is_ok());
        let payload = payload_result.unwrap();

        // Serialize the actual payload to JSON string
        let actual_json = serde_json::to_string_pretty(&payload).unwrap();

        // Define the expected JSON using the group_id and the actual_event_id
        let expected_json = format!(
            r#"{{
            "notification": {{
              "title": "New message from npub10xlxvlh",
              "body": "This is a test group message mentioning someone. It has more than 150 characters to ensure that the truncation logic is tested properly. Let's add eve"
            }},
            "data": {{
              "groupId": "{}",
              "nostrEventId": "{}"
            }}
          }}"#,
            group_id, actual_event_id
        );

        // Parse both JSON strings back into serde_json::Value for comparison
        let actual_value: serde_json::Value = serde_json::from_str(&actual_json).unwrap();
        let expected_value: serde_json::Value = serde_json::from_str(&expected_json).unwrap();

        // Compare the serde_json::Value objects
        assert_eq!(actual_value, expected_value);
    }
}
