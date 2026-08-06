use crate::message::ContentBlock;
use crate::session::{Session, StoredDisplayRole, StoredMessage};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_MATCHING_SESSIONS: usize = 12;
const MAX_CANDIDATE_FILES_INSPECTED: usize = 256;
const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MESSAGES_PER_SESSION: usize = 500;
const MAX_RECENT_ACTIVITIES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerActivityDirection {
    Inbound,
    Outbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerActivity {
    pub occurred_at: Option<DateTime<Utc>>,
    pub direction: PeerActivityDirection,
    pub peer_alias: String,
    pub peer_project: Option<String>,
    pub outcome: PeerActivityOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerActivityReport {
    pub activities: Vec<PeerActivity>,
    pub history_limited: bool,
    pub read_errors: usize,
}

#[derive(Debug)]
struct SessionCandidate {
    path: PathBuf,
    modified: SystemTime,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct SessionWorkspaceMetadata {
    #[serde(default)]
    working_dir: Option<String>,
}

#[derive(Debug)]
struct RankedActivity {
    activity: PeerActivity,
    sort_at: DateTime<Utc>,
    session_order: usize,
    activity_order: usize,
}

fn candidate_files(sessions_dir: &Path) -> Result<Vec<SessionCandidate>> {
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(sessions_dir)?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        candidates.push(SessionCandidate {
            path,
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: metadata.len(),
        });
    }
    candidates.sort_unstable_by(|left, right| right.modified.cmp(&left.modified));
    Ok(candidates)
}

fn snapshot_workspace(path: &Path) -> Result<Option<PathBuf>> {
    let reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let metadata: SessionWorkspaceMetadata = serde_json::from_reader(reader)?;
    let Some(working_dir) = metadata.working_dir else {
        return Ok(None);
    };
    Ok(Some(std::fs::canonicalize(working_dir)?))
}

pub fn load_recent_peer_activity(canonical_working_dir: &Path) -> Result<PeerActivityReport> {
    let sessions_dir = crate::storage::jcode_dir()?.join("sessions");
    let candidates = candidate_files(&sessions_dir)?;
    let mut matching = Vec::new();
    let mut history_limited = false;
    let mut read_errors = 0;

    for (candidate_index, candidate) in candidates.into_iter().enumerate() {
        if candidate_index == MAX_CANDIDATE_FILES_INSPECTED {
            history_limited = true;
            break;
        }
        if candidate.size > MAX_SNAPSHOT_BYTES {
            history_limited = true;
            continue;
        }
        match snapshot_workspace(&candidate.path) {
            Ok(Some(workspace)) if workspace == canonical_working_dir => {
                matching.push(candidate.path);
                if matching.len() == MAX_MATCHING_SESSIONS {
                    history_limited = true;
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => read_errors += 1,
        }
    }

    let mut ranked = Vec::new();
    for (session_order, path) in matching.iter().enumerate() {
        let session = match Session::load_from_path(path) {
            Ok(session) => session,
            Err(_) => {
                read_errors += 1;
                continue;
            }
        };
        let message_start = session
            .messages
            .len()
            .saturating_sub(MAX_MESSAGES_PER_SESSION);
        for (activity_order, activity) in
            extract_peer_activities(&session.messages[message_start..])
                .into_iter()
                .enumerate()
        {
            ranked.push(RankedActivity {
                sort_at: activity.occurred_at.unwrap_or(session.updated_at),
                activity,
                session_order,
                activity_order,
            });
        }
    }
    ranked.sort_by(|left, right| {
        right
            .sort_at
            .cmp(&left.sort_at)
            .then_with(|| left.session_order.cmp(&right.session_order))
            .then_with(|| right.activity_order.cmp(&left.activity_order))
    });

    let mut seen = HashSet::new();
    let mut activities = Vec::new();
    for item in ranked {
        if seen.insert(item.activity.clone()) {
            activities.push(item.activity);
            if activities.len() == MAX_RECENT_ACTIVITIES {
                break;
            }
        }
    }
    Ok(PeerActivityReport {
        activities,
        history_limited,
        read_errors,
    })
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
    use crate::session::{Session, StoredDisplayRole};
    use chrono::{Duration, TimeZone};
    use serde_json::json;
    use std::path::Path;

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

    fn with_temp_home<T>(test: impl FnOnce(&Path) -> T) -> T {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create temp home");
        let previous_home = std::env::var("JCODE_HOME").ok();
        crate::env::set_var("JCODE_HOME", temp.path());
        std::fs::create_dir_all(temp.path().join("sessions")).expect("create sessions dir");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(temp.path())));
        if let Some(previous_home) = previous_home {
            crate::env::set_var("JCODE_HOME", previous_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
        result.unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    }

    fn save_activity_session(
        id: &str,
        working_dir: &Path,
        timestamp: DateTime<Utc>,
        activity: Option<StoredMessage>,
    ) {
        let mut session = Session::create_with_id(id.to_string(), None, None);
        session.working_dir = Some(working_dir.to_string_lossy().into_owned());
        session.updated_at = timestamp;
        if let Some(activity) = activity {
            session.messages.push(activity);
        }
        session.save().expect("save activity session");
    }

    fn inbound(alias: &str, project: &str, timestamp: DateTime<Utc>) -> StoredMessage {
        StoredMessage {
            id: format!("inbound-{alias}"),
            role: Role::User,
            content: vec![text(&format!(
                "Verified peer message from {alias} (`{project}`) to Jcode (`jcode`).\nMessage ID: `private-id`\n\nPRIVATE BODY"
            ))],
            display_role: Some(StoredDisplayRole::Peer),
            timestamp: Some(timestamp),
            tool_duration_ms: None,
            token_usage: None,
        }
    }

    #[test]
    fn recent_peer_activity_is_workspace_scoped_newest_first_and_capped_at_five() {
        with_temp_home(|_| {
            let workspace = tempfile::TempDir::new().expect("workspace");
            let other = tempfile::TempDir::new().expect("other workspace");
            let canonical = workspace
                .path()
                .canonicalize()
                .expect("canonical workspace");
            let base = Utc.with_ymd_and_hms(2026, 8, 6, 12, 0, 0).unwrap();
            for index in 0..7 {
                let timestamp = base + Duration::minutes(index);
                save_activity_session(
                    &format!("matching-{index}"),
                    workspace.path(),
                    timestamp,
                    Some(inbound(&format!("Peer{index}"), "approved", timestamp)),
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            let other_time = base + Duration::hours(2);
            save_activity_session(
                "different-workspace",
                other.path(),
                other_time,
                Some(inbound("WrongWorkspace", "private", other_time)),
            );

            let report = load_recent_peer_activity(&canonical).expect("load recent activity");

            assert_eq!(report.activities.len(), 5);
            assert_eq!(
                report
                    .activities
                    .iter()
                    .map(|activity| activity.peer_alias.as_str())
                    .collect::<Vec<_>>(),
                vec!["Peer6", "Peer5", "Peer4", "Peer3", "Peer2"]
            );
            assert!(!report.history_limited);
            assert_eq!(report.read_errors, 0);
        });
    }

    #[test]
    fn recent_peer_activity_applies_session_message_size_and_error_bounds() {
        with_temp_home(|home| {
            let workspace = tempfile::TempDir::new().expect("workspace");
            let canonical = workspace
                .path()
                .canonicalize()
                .expect("canonical workspace");
            let base = Utc.with_ymd_and_hms(2026, 8, 6, 12, 0, 0).unwrap();

            let old_future_activity =
                inbound("TooOldSession", "approved", base + Duration::days(1));
            save_activity_session(
                "oldest-matching",
                workspace.path(),
                base,
                Some(old_future_activity),
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
            for index in 0..12 {
                save_activity_session(
                    &format!("newer-empty-{index}"),
                    workspace.path(),
                    base + Duration::minutes(index + 1),
                    None,
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }

            let mut many_messages =
                Session::create_with_id("message-bound".to_string(), None, None);
            many_messages.working_dir = Some(workspace.path().to_string_lossy().into_owned());
            many_messages
                .messages
                .push(inbound("OutsideNewest500", "approved", base));
            for index in 0..500 {
                many_messages.messages.push(StoredMessage {
                    id: format!("ordinary-{index}"),
                    role: Role::Assistant,
                    content: vec![text("ordinary")],
                    display_role: None,
                    timestamp: Some(base + Duration::seconds(index)),
                    tool_duration_ms: None,
                    token_usage: None,
                });
            }
            many_messages.save().expect("save bounded-message session");

            std::fs::write(home.join("sessions").join("malformed.json"), b"{not-json")
                .expect("write malformed snapshot");
            std::fs::write(
                home.join("sessions").join("oversized.json"),
                vec![b' '; 2 * 1024 * 1024 + 1],
            )
            .expect("write oversized snapshot");

            let report = load_recent_peer_activity(&canonical).expect("bounded scan");

            assert!(report.history_limited);
            assert!(report.read_errors >= 1);
            assert!(report.activities.iter().all(|activity| {
                activity.peer_alias != "TooOldSession" && activity.peer_alias != "OutsideNewest500"
            }));
        });
    }

    #[test]
    fn recent_peer_activity_stops_after_bounded_unrelated_candidates() {
        with_temp_home(|_| {
            let workspace = tempfile::TempDir::new().expect("workspace");
            let other = tempfile::TempDir::new().expect("other workspace");
            let canonical = workspace
                .path()
                .canonicalize()
                .expect("canonical workspace");
            let base = Utc.with_ymd_and_hms(2026, 8, 6, 12, 0, 0).unwrap();

            save_activity_session(
                "older-match",
                workspace.path(),
                base,
                Some(inbound("OlderMatch", "approved", base)),
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
            for index in 0..MAX_CANDIDATE_FILES_INSPECTED {
                save_activity_session(
                    &format!("newer-unrelated-{index}"),
                    other.path(),
                    base + Duration::minutes(index as i64 + 1),
                    None,
                );
            }

            let report = load_recent_peer_activity(&canonical).expect("bounded candidate scan");

            assert!(report.history_limited);
            assert!(report.activities.is_empty());
            assert_eq!(report.read_errors, 0);
        });
    }

    #[test]
    fn recent_peer_activity_reports_when_the_session_scan_reaches_its_cap() {
        with_temp_home(|_| {
            let workspace = tempfile::TempDir::new().expect("workspace");
            let canonical = workspace
                .path()
                .canonicalize()
                .expect("canonical workspace");
            let base = Utc.with_ymd_and_hms(2026, 8, 6, 12, 0, 0).unwrap();
            for index in 0..MAX_MATCHING_SESSIONS {
                save_activity_session(
                    &format!("matching-cap-{index}"),
                    workspace.path(),
                    base + Duration::minutes(index as i64),
                    None,
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }

            let report = load_recent_peer_activity(&canonical).expect("bounded scan");

            assert!(report.history_limited);
            assert_eq!(report.read_errors, 0);
        });
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
