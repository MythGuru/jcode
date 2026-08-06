use crate::message::ContentBlock;
use crate::session::{StoredDisplayRole, StoredMessage};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerActivityDirection {
    Inbound,
    Outbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerActivityOutcome {
    Sent,
    Replied,
    CompletedWithoutReply,
    Failed,
    TimedOut,
    Cancelled,
    Received,
    OutcomeUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerActivity {
    pub occurred_at: Option<DateTime<Utc>>,
    pub direction: PeerActivityDirection,
    pub peer_alias: String,
    pub peer_project: Option<String>,
    pub outcome: PeerActivityOutcome,
}

#[derive(Debug, Clone)]
struct ClassifiedResult {
    peer_alias: String,
    peer_project: Option<String>,
    outcome: PeerActivityOutcome,
}

fn safe_label(value: &str, max_chars: usize) -> Option<String> {
    let value = value.trim();
    let char_count = value.chars().count();
    if char_count == 0
        || char_count > max_chars
        || value.chars().any(|character| character.is_control())
    {
        return None;
    }
    Some(value.to_string())
}

fn parse_peer_reference(value: &str) -> Option<(String, String)> {
    let (alias, project_and_rest) = value.split_once(" (`")?;
    let (project, _) = project_and_rest.split_once("`)")?;
    Some((safe_label(alias, 100)?, safe_label(project, 160)?))
}

fn classify_peer_result(content: &str) -> Option<ClassifiedResult> {
    let heading = content.lines().next()?.trim();
    let (reference, outcome) = if let Some(reference) = heading.strip_prefix("Peer reply from ") {
        (reference, PeerActivityOutcome::Replied)
    } else if let Some(reference) = heading.strip_prefix("Peer turn with ") {
        if heading.ends_with(" failed.") {
            (
                reference.strip_suffix(" failed.")?,
                PeerActivityOutcome::Failed,
            )
        } else if heading.ends_with(" timed out.") {
            (
                reference.strip_suffix(" timed out.")?,
                PeerActivityOutcome::TimedOut,
            )
        } else if heading.ends_with(" was cancelled.") {
            (
                reference.strip_suffix(" was cancelled.")?,
                PeerActivityOutcome::Cancelled,
            )
        } else {
            return None;
        }
    } else if let Some(reference) = heading.strip_prefix("Peer message sent to ") {
        (reference, PeerActivityOutcome::Sent)
    } else if heading.ends_with(" completed the peer turn without replying.") {
        (
            heading.strip_suffix(" completed the peer turn without replying.")?,
            PeerActivityOutcome::CompletedWithoutReply,
        )
    } else {
        return None;
    };
    let reference = reference.strip_suffix('.').unwrap_or(reference);
    let (peer_alias, peer_project) = parse_peer_reference(reference)?;
    Some(ClassifiedResult {
        peer_alias,
        peer_project: Some(peer_project),
        outcome,
    })
}

fn inbound_peer_identity(message: &StoredMessage) -> (String, Option<String>) {
    let verified = message.content.iter().find_map(|block| match block {
        ContentBlock::Text { text, .. } => text
            .lines()
            .next()
            .and_then(|line| line.trim().strip_prefix("Verified peer message from ")),
        _ => None,
    });
    verified
        .and_then(parse_peer_reference)
        .map(|(alias, project)| (alias, Some(project)))
        .unwrap_or_else(|| ("Unknown peer".to_string(), None))
}

pub fn extract_peer_activities(messages: &[StoredMessage]) -> Vec<PeerActivity> {
    let mut results = HashMap::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
                && let Some(classified) = classify_peer_result(content)
            {
                results.entry(tool_use_id.clone()).or_insert(classified);
            }
        }
    }

    let mut activities = Vec::new();
    for message in messages {
        if message.display_role == Some(StoredDisplayRole::Peer) {
            let (peer_alias, peer_project) = inbound_peer_identity(message);
            activities.push(PeerActivity {
                occurred_at: message.timestamp,
                direction: PeerActivityDirection::Inbound,
                peer_alias,
                peer_project,
                outcome: PeerActivityOutcome::Received,
            });
        }

        for block in &message.content {
            let ContentBlock::ToolUse {
                id, name, input, ..
            } = block
            else {
                continue;
            };
            if name != "peer"
                || input.get("action").and_then(|value| value.as_str()) != Some("send")
            {
                continue;
            }
            let Some(input_alias) = input
                .get("to")
                .and_then(|value| value.as_str())
                .and_then(|value| safe_label(value, 100))
            else {
                continue;
            };
            let classified = results.get(id);
            activities.push(PeerActivity {
                occurred_at: message.timestamp,
                direction: PeerActivityDirection::Outbound,
                peer_alias: classified
                    .map(|result| result.peer_alias.clone())
                    .unwrap_or(input_alias),
                peer_project: classified.and_then(|result| result.peer_project.clone()),
                outcome: classified
                    .map(|result| result.outcome)
                    .unwrap_or(PeerActivityOutcome::OutcomeUnavailable),
            });
        }
    }
    activities
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Role};
    use crate::session::StoredDisplayRole;
    use chrono::TimeZone;
    use serde_json::json;

    fn stored(
        id: &str,
        role: Role,
        display_role: Option<StoredDisplayRole>,
        minute: u32,
        content: Vec<ContentBlock>,
    ) -> StoredMessage {
        StoredMessage {
            id: id.to_string(),
            role,
            content,
            display_role,
            timestamp: Some(Utc.with_ymd_and_hms(2026, 8, 6, 18, minute, 0).unwrap()),
            tool_duration_ms: None,
            token_usage: None,
        }
    }

    fn text(value: &str) -> ContentBlock {
        ContentBlock::Text {
            text: value.to_string(),
            cache_control: None,
        }
    }

    fn peer_send(id: &str, to: &str, body: &str, minute: u32) -> StoredMessage {
        stored(
            id,
            Role::Assistant,
            None,
            minute,
            vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "peer".to_string(),
                input: json!({
                    "action": "send",
                    "to": to,
                    "message": body,
                    "tldr": "private summary"
                }),
                thought_signature: None,
            }],
        )
    }

    fn peer_result(id: &str, content: &str, minute: u32) -> StoredMessage {
        stored(
            &format!("result-{id}"),
            Role::User,
            None,
            minute,
            vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
                is_error: None,
            }],
        )
    }

    #[test]
    fn peer_activity_extracts_verified_inbound_identity_without_body_text() {
        let secret = "PRIVATE BODY MUST NEVER APPEAR";
        let messages = vec![stored(
            "inbound",
            Role::User,
            Some(StoredDisplayRole::Peer),
            1,
            vec![text(&format!(
                "Verified peer message from Atlas (`healthview-platform`) to Jcode (`jcode`).\nMessage ID: `peer_1`\n\n{secret}"
            ))],
        )];

        let activities = extract_peer_activities(&messages);

        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0].direction, PeerActivityDirection::Inbound);
        assert_eq!(activities[0].outcome, PeerActivityOutcome::Received);
        assert_eq!(activities[0].peer_alias, "Atlas");
        assert_eq!(
            activities[0].peer_project.as_deref(),
            Some("healthview-platform")
        );
        assert!(!format!("{activities:?}").contains(secret));
    }

    #[test]
    fn peer_activity_pairs_outbound_send_results_and_classifies_every_outcome() {
        let private_body = "PRIVATE OUTBOUND BODY";
        let cases = [
            (
                "reply",
                "Peer reply from Atlas (`atlas-project`).\n\nMessage ID: `peer_1`\n\nPRIVATE REPLY",
                PeerActivityOutcome::Replied,
            ),
            (
                "complete",
                "Atlas (`atlas-project`) completed the peer turn without replying.\n\nMessage ID: `peer_2`",
                PeerActivityOutcome::CompletedWithoutReply,
            ),
            (
                "failed",
                "Peer turn with Atlas (`atlas-project`) failed.\n\nMessage ID: `peer_3`\n\nPRIVATE ERROR",
                PeerActivityOutcome::Failed,
            ),
            (
                "timeout",
                "Peer turn with Atlas (`atlas-project`) timed out.\n\nMessage ID: `peer_4`",
                PeerActivityOutcome::TimedOut,
            ),
            (
                "cancel",
                "Peer turn with Atlas (`atlas-project`) was cancelled.\n\nMessage ID: `peer_5`",
                PeerActivityOutcome::Cancelled,
            ),
            (
                "sent",
                "Peer message sent to Atlas (`atlas-project`).",
                PeerActivityOutcome::Sent,
            ),
        ];
        let mut messages = Vec::new();
        for (index, (id, result, _)) in cases.iter().enumerate() {
            messages.push(peer_send(id, "Atlas", private_body, index as u32));
            messages.push(peer_result(id, result, index as u32));
        }
        messages.push(peer_send("missing", "Planner", private_body, 10));

        let activities = extract_peer_activities(&messages);

        assert_eq!(activities.len(), cases.len() + 1);
        for (activity, (_, _, expected)) in activities.iter().zip(cases) {
            assert_eq!(activity.direction, PeerActivityDirection::Outbound);
            assert_eq!(activity.peer_alias, "Atlas");
            assert_eq!(activity.peer_project.as_deref(), Some("atlas-project"));
            assert_eq!(activity.outcome, expected);
        }
        assert_eq!(
            activities.last().expect("missing result activity").outcome,
            PeerActivityOutcome::OutcomeUnavailable
        );
        let rendered = format!("{activities:?}");
        assert!(!rendered.contains(private_body));
        assert!(!rendered.contains("PRIVATE REPLY"));
        assert!(!rendered.contains("PRIVATE ERROR"));
    }

    #[test]
    fn peer_activity_ignores_non_send_actions_and_handles_malformed_legacy_blocks() {
        let messages = vec![
            stored(
                "list",
                Role::Assistant,
                None,
                1,
                vec![ContentBlock::ToolUse {
                    id: "list".to_string(),
                    name: "peer".to_string(),
                    input: json!({"action": "list"}),
                    thought_signature: None,
                }],
            ),
            stored(
                "malformed",
                Role::Assistant,
                None,
                2,
                vec![ContentBlock::ToolUse {
                    id: "malformed".to_string(),
                    name: "peer".to_string(),
                    input: json!({"action": "send", "to": 42, "message": ["legacy"]}),
                    thought_signature: None,
                }],
            ),
            stored(
                "unknown-peer",
                Role::User,
                Some(StoredDisplayRole::Peer),
                3,
                vec![text("legacy peer body without a verified heading")],
            ),
        ];

        let activities = extract_peer_activities(&messages);

        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0].peer_alias, "Unknown peer");
        assert!(activities[0].peer_project.is_none());
    }
}
