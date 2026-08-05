use super::{Tool, ToolContext, ToolExecutionMode, TurnExecutionContext, TurnOrigin};
use crate::peer_timing::peer_socket_timeout;
use crate::protocol::{
    PeerCaller, PeerInfo, PeerOutcome, PeerResult, PeerState, Request, ServerEvent,
};
use anyhow::Result;
use async_trait::async_trait;
use jcode_swarm_core::validate_swarm_tldr;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

const REQUEST_ID: u64 = 1;
const PEER_MESSAGE_MAX_CHARS: usize = 8_000;

pub struct PeerTool;

impl PeerTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Clone, Deserialize)]
struct PeerInput {
    action: String,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    tldr: Option<String>,
}

fn validated_caller(ctx: &ToolContext) -> Result<(&TurnExecutionContext, PeerCaller)> {
    let ToolExecutionMode::AgentTurn(turn) = &ctx.execution_mode else {
        anyhow::bail!("This tool call does not have a valid live server turn capability.");
    };
    let (Some(session_id), Some(generation), Some(capability)) = (
        turn.server_session_id.as_ref(),
        turn.turn_generation,
        turn.turn_capability.as_ref(),
    ) else {
        anyhow::bail!("This tool call does not have a valid live server turn capability.");
    };
    Ok((
        turn,
        PeerCaller {
            session_id: session_id.clone(),
            generation,
            capability: capability.expose_secret().to_string(),
        },
    ))
}

fn required_message(input: &PeerInput, action: &str) -> Result<String> {
    let message = input
        .message
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("'message' is required for peer {action}"))?
        .trim();
    if message.is_empty() {
        anyhow::bail!("Peer message must not be empty.");
    }
    if message.chars().count() > PEER_MESSAGE_MAX_CHARS {
        anyhow::bail!("Peer message must be at most 8,000 characters.");
    }
    Ok(message.to_string())
}

fn build_peer_request(input: PeerInput, ctx: &ToolContext) -> Result<Request> {
    let (turn, caller) = validated_caller(ctx)?;
    match input.action.trim().to_ascii_lowercase().as_str() {
        "list" => Ok(Request::PeerList {
            id: REQUEST_ID,
            caller,
        }),
        "send" => {
            if !matches!(turn.origin, TurnOrigin::NormalUser) {
                anyhow::bail!(
                    "Peer messages can only be started during a normal user-directed turn."
                );
            }
            let to = input
                .to
                .as_deref()
                .map(str::trim)
                .filter(|target| !target.is_empty())
                .ok_or_else(|| anyhow::anyhow!("'to' is required for peer send"))?
                .to_string();
            let message = required_message(&input, "send")?;
            let tldr = validate_swarm_tldr(input.tldr.as_deref(), &message, "this peer message")
                .map_err(|error| anyhow::anyhow!(error))?;
            Ok(Request::PeerSend {
                id: REQUEST_ID,
                caller,
                to,
                message,
                tldr,
            })
        }
        "reply" => {
            if !matches!(turn.origin, TurnOrigin::PeerInbound { .. }) {
                anyhow::bail!("This turn cannot start or reply to peer messages.");
            }
            Ok(Request::PeerReply {
                id: REQUEST_ID,
                caller,
                message: required_message(&input, "reply")?,
            })
        }
        action => anyhow::bail!("Unknown peer action '{action}'. Use list, send, or reply."),
    }
}

fn format_peer_state(state: PeerState) -> &'static str {
    match state {
        PeerState::Idle => "idle",
        PeerState::Busy => "busy",
        PeerState::Offline => "offline",
        PeerState::Ambiguous => "ambiguous",
    }
}

fn format_peer_list(peers: Vec<PeerInfo>) -> String {
    if peers.is_empty() {
        return "No configured peers are visible to this project.".to_string();
    }
    peers
        .into_iter()
        .map(|peer| {
            format!(
                "- {} (`{}`) · group `{}` · {}",
                peer.alias,
                peer.project,
                peer.group,
                format_peer_state(peer.state)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_peer_result(result: PeerResult) -> String {
    let peer = format!("{} (`{}`)", result.from, result.from_project);
    let heading = match result.status {
        PeerOutcome::Replied => format!("Peer reply from {peer}"),
        PeerOutcome::CompletedWithoutReply => {
            format!("{peer} completed the peer turn without replying")
        }
        PeerOutcome::Failed => format!("Peer turn with {peer} failed"),
        PeerOutcome::TimedOut => format!("Peer turn with {peer} timed out"),
        PeerOutcome::Cancelled => format!("Peer turn with {peer} was cancelled"),
    };
    let mut output = format!("{heading}.\n\nMessage ID: `{}`", result.message_id);
    if let Some(reply) = result.reply {
        output.push_str("\n\n");
        output.push_str(&reply);
    }
    if let Some(error) = result.error {
        output.push_str("\n\n");
        output.push_str(&error);
    }
    output
}

async fn best_effort_cancel(caller: PeerCaller) {
    let request = Request::PeerCancel {
        id: REQUEST_ID,
        caller,
    };
    let _ = super::communicate::transport::send_request_with_timeout(
        request,
        Some(Duration::from_secs(5)),
    )
    .await;
}

#[async_trait]
impl Tool for PeerTool {
    fn name(&self) -> &str {
        "peer"
    }

    fn description(&self) -> &str {
        "Message an allowlisted peer project once, or reply once to an inbound peer."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "send", "reply"],
                    "description": "List visible peers, send one message, or reply once to an inbound peer message."
                },
                "to": {
                    "type": "string",
                    "description": "Configured peer alias. Required only for send."
                },
                "message": {
                    "type": "string",
                    "maxLength": PEER_MESSAGE_MAX_CHARS,
                    "description": "Message body. Required for send and reply."
                },
                "tldr": {
                    "type": "string",
                    "description": "One-line summary under ~120 chars. Required for send bodies longer than 240 chars."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<super::ToolOutput> {
        if !crate::config::config().features.peer_messaging {
            anyhow::bail!("Peer messaging is disabled.");
        }
        let input: PeerInput = serde_json::from_value(input)?;
        let request = build_peer_request(input, &ctx)?;

        match request {
            Request::PeerSend { ref caller, .. } => {
                let shutdown = ctx.graceful_shutdown_signal.clone().ok_or_else(|| {
                    anyhow::anyhow!("This tool call does not have a cancellable live server turn.")
                })?;
                let caller = caller.clone();
                let mut pending =
                    Box::pin(super::communicate::transport::send_request_with_timeout(
                        request,
                        Some(peer_socket_timeout()),
                    ));
                tokio::select! {
                    response = &mut pending => match response? {
                        ServerEvent::PeerSendResult { result, .. } => {
                            Ok(super::ToolOutput::new(format_peer_result(result)))
                        }
                        ServerEvent::Error { message, .. } => Err(anyhow::anyhow!(message)),
                        event => Err(anyhow::anyhow!("Unexpected peer send response: {event:?}")),
                    },
                    _ = shutdown.notified() => {
                        best_effort_cancel(caller).await;
                        Err(anyhow::anyhow!(
                            "The peer exchange was cancelled before a reply was delivered."
                        ))
                    }
                }
            }
            request @ Request::PeerList { .. } => {
                match super::communicate::transport::send_request(request).await? {
                    ServerEvent::PeerListResult { peers, .. } => {
                        Ok(super::ToolOutput::new(format_peer_list(peers)))
                    }
                    ServerEvent::Error { message, .. } => Err(anyhow::anyhow!(message)),
                    event => Err(anyhow::anyhow!("Unexpected peer list response: {event:?}")),
                }
            }
            request @ Request::PeerReply { .. } => {
                match super::communicate::transport::send_request(request).await? {
                    ServerEvent::PeerReplyAccepted { message_id, .. } => Ok(
                        super::ToolOutput::new(format!("Peer reply recorded for `{message_id}`.")),
                    ),
                    ServerEvent::Error { message, .. } => Err(anyhow::anyhow!(message)),
                    event => Err(anyhow::anyhow!("Unexpected peer reply response: {event:?}")),
                }
            }
            Request::PeerCancel { .. } => unreachable!("cancel is internal only"),
            _ => unreachable!("peer request builder returned a non-peer request"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PeerCaller, Request};
    use crate::tool::{Tool, ToolContext, ToolExecutionMode, TurnCapability, TurnExecutionContext};

    fn live_ctx(turn: TurnExecutionContext) -> ToolContext {
        ToolContext {
            session_id: "visible-session-label".to_string(),
            message_id: "message".to_string(),
            tool_call_id: "tool-call".to_string(),
            working_dir: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::AgentTurn(turn),
        }
    }

    #[test]
    fn peer_schema_exposes_only_list_send_and_reply_inputs() {
        let schema = PeerTool::new().parameters_schema();
        let serialized = serde_json::to_string(&schema).expect("schema serializes");

        for action in ["list", "send", "reply"] {
            assert!(serialized.contains(&format!("\"{action}\"")));
        }
        for hidden in [
            "session_id",
            "generation",
            "capability",
            "working_dir",
            "exchange_id",
            "from",
        ] {
            assert!(
                !serialized.contains(hidden),
                "model schema leaked hidden peer field {hidden}: {serialized}"
            );
        }
    }

    #[test]
    fn peer_socket_timeout_is_derived_from_shared_server_deadline() {
        assert_eq!(
            crate::peer_timing::peer_socket_timeout(),
            crate::peer_timing::PEER_RECIPIENT_DEADLINE
                .saturating_add(std::time::Duration::from_secs(30))
        );
    }

    #[test]
    fn peer_result_identifies_reply_project() {
        let output = format_peer_result(PeerResult {
            status: PeerOutcome::Replied,
            message_id: "peer_123".to_string(),
            from: "Atlas".to_string(),
            from_project: "healthview-platform".to_string(),
            to: "Eve".to_string(),
            to_project: "healthview-app".to_string(),
            reply: Some("Reviewed.".to_string()),
            error: None,
        });

        assert!(output.starts_with("Peer reply from Atlas (`healthview-platform`)."));
        assert!(output.contains("Reviewed."));
    }

    #[test]
    fn peer_request_uses_only_hidden_live_turn_identity() {
        let ctx = live_ctx(TurnExecutionContext::normal_user(
            "server-session".to_string(),
            42,
            TurnCapability::new("secret-capability".to_string()),
        ));
        let request = build_peer_request(
            PeerInput {
                action: "send".to_string(),
                to: Some("Atlas".to_string()),
                message: Some("Please review this.".to_string()),
                tldr: None,
            },
            &ctx,
        )
        .expect("valid live peer request");

        assert!(matches!(
            request,
            Request::PeerSend {
                caller: PeerCaller {
                    session_id,
                    generation: 42,
                    capability,
                },
                to,
                message,
                ..
            } if session_id == "server-session"
                && capability == "secret-capability"
                && to == "Atlas"
                && message == "Please review this."
        ));
    }

    #[test]
    fn direct_or_standalone_context_is_rejected_before_transport() {
        let direct = ToolContext {
            session_id: "direct".to_string(),
            message_id: "message".to_string(),
            tool_call_id: "tool-call".to_string(),
            working_dir: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        };
        let standalone = live_ctx(TurnExecutionContext::standalone("test"));
        let input = PeerInput {
            action: "list".to_string(),
            to: None,
            message: None,
            tldr: None,
        };

        for ctx in [&direct, &standalone] {
            let error = build_peer_request(input.clone(), ctx).expect_err("must reject");
            assert_eq!(
                error.to_string(),
                "This tool call does not have a valid live server turn capability."
            );
        }
    }

    #[test]
    fn peer_send_validates_body_length_and_swarm_tldr_rule() {
        let ctx = live_ctx(TurnExecutionContext::normal_user(
            "server-session".to_string(),
            7,
            TurnCapability::new("capability".to_string()),
        ));

        let blank = build_peer_request(
            PeerInput {
                action: "send".to_string(),
                to: Some("Atlas".to_string()),
                message: Some("   ".to_string()),
                tldr: None,
            },
            &ctx,
        )
        .expect_err("blank message must fail");
        assert!(blank.to_string().contains("must not be empty"));

        let too_long = build_peer_request(
            PeerInput {
                action: "send".to_string(),
                to: Some("Atlas".to_string()),
                message: Some("x".repeat(8_001)),
                tldr: Some("Long review".to_string()),
            },
            &ctx,
        )
        .expect_err("oversized message must fail");
        assert!(too_long.to_string().contains("8,000"));

        let missing_tldr = build_peer_request(
            PeerInput {
                action: "send".to_string(),
                to: Some("Atlas".to_string()),
                message: Some("x".repeat(241)),
                tldr: None,
            },
            &ctx,
        )
        .expect_err("long message requires tldr");
        assert!(missing_tldr.to_string().contains("tldr"));
    }
}
