use crate::protocol::{ServerEvent, encode_event};
use anyhow::Result;
use std::fmt::{Display, Write as _};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

pub(super) async fn write_direct_event(
    writer: &Arc<Mutex<crate::transport::WriteHalf>>,
    event: &ServerEvent,
) -> Result<()> {
    let json = encode_event(event);
    let mut w = writer.lock().await;
    w.write_all(json.as_bytes()).await?;
    Ok(())
}

pub(super) fn log_event_write_failure(
    source: &str,
    connection_id: Option<&str>,
    event: &ServerEvent,
    error: &dyn Display,
) {
    emit_event_write_failure(source, connection_id, event, error, crate::logging::warn);
}

fn emit_event_write_failure(
    source: &str,
    connection_id: Option<&str>,
    event: &ServerEvent,
    error: &dyn Display,
    mut emit: impl FnMut(&str),
) {
    let (event_kind, request_id) = safe_event_log_metadata(event);
    let mut message = format!("client_event_write_failed source={source}");
    if let Some(connection_id) = connection_id {
        let _ = write!(message, " connection_id={connection_id}");
    }
    let _ = write!(message, " event_kind={event_kind}");
    if let Some(request_id) = request_id {
        let _ = write!(message, " request_id={request_id}");
    }
    let _ = write!(message, " error={error}");
    emit(&message);
}

fn safe_event_log_metadata(event: &ServerEvent) -> (String, Option<u64>) {
    let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(event) else {
        return ("unknown".to_string(), None);
    };
    let event_kind = fields
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let request_id = fields.get("id").and_then(serde_json::Value::as_u64);
    (event_kind, request_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PeerOutcome, PeerResult};

    #[test]
    fn event_write_failure_log_excludes_peer_payload() {
        let secret = "peer-secret-reply-that-must-never-reach-logs";
        let event = ServerEvent::PeerSendResult {
            id: 42,
            result: PeerResult {
                status: PeerOutcome::Replied,
                message_id: "peer-message-7".to_string(),
                from: "sender-session".to_string(),
                from_project: "sender-project".to_string(),
                to: "recipient-session".to_string(),
                to_project: "recipient-project".to_string(),
                reply: Some(secret.to_string()),
                error: None,
            },
        };
        let mut captured = Vec::new();

        emit_event_write_failure(
            "lightweight_control",
            None,
            &event,
            &std::io::Error::new(std::io::ErrorKind::BrokenPipe, "socket closed"),
            |message| captured.push(message.to_string()),
        );

        let captured = captured.join("\n");
        assert!(captured.contains("event_kind=peer_send_result"));
        assert!(captured.contains("request_id=42"));
        assert!(!captured.contains(secret));
        assert!(!captured.contains("sender-session"));
        assert!(!captured.contains("sender-project"));
        assert!(!captured.contains("recipient-session"));
        assert!(!captured.contains("recipient-project"));
    }
}
