use super::Server;
use super::turn_coordinator::TurnValidationError;
use crate::message::{ContentBlock, Message, StreamEvent, ToolDefinition};
use crate::protocol::{HistoryMessage, PeerCaller, Request, ServerEvent};
use crate::provider::{EventStream, Provider};
use crate::session::{Session, StoredDisplayRole};
use crate::tool::TurnOrigin;
use crate::transport::{ReadHalf, Stream, WriteHalf};
use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

struct EnvGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let original = std::env::var_os(key);
        crate::env::set_var(key, value);
        jcode_base::config::invalidate_config_cache();
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(value) = self.original.take() {
            crate::env::set_var(self.key, value);
        } else {
            crate::env::remove_var(self.key);
        }
        jcode_base::config::invalidate_config_cache();
    }
}

struct RawClient {
    reader: BufReader<ReadHalf>,
    writer: WriteHalf,
    next_id: u64,
}

impl RawClient {
    async fn connect(path: &Path) -> Result<Self> {
        let stream = Stream::connect(path).await?;
        let (reader, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
        })
    }

    async fn send_request(&mut self, request: Request) -> Result<u64> {
        let id = request.id();
        let json = serde_json::to_string(&request)? + "\n";
        self.writer.write_all(json.as_bytes()).await?;
        Ok(id)
    }

    async fn read_event(&mut self) -> Result<ServerEvent> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line).await?;
        if read == 0 {
            anyhow::bail!("server disconnected")
        }
        Ok(serde_json::from_str(&line)?)
    }

    async fn read_until<F>(&mut self, duration: Duration, mut predicate: F) -> Result<ServerEvent>
    where
        F: FnMut(&ServerEvent) -> bool,
    {
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = tokio::time::timeout(remaining, self.read_event()).await??;
            if predicate(&event) {
                return Ok(event);
            }
        }
    }

    async fn subscribe(&mut self, working_dir: &Path) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_request(Request::Subscribe {
            id,
            working_dir: Some(working_dir.display().to_string()),
            selfdev: None,
            target_session_id: None,
            client_instance_id: None,
            client_has_local_history: false,
            allow_session_takeover: false,
            terminal_env: Vec::new(),
        })
        .await?;
        self.read_until(
            Duration::from_secs(5),
            |event| matches!(event, ServerEvent::Done { id: done_id } if *done_id == id),
        )
        .await?;
        Ok(())
    }

    async fn session_id(&mut self) -> Result<String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_request(Request::GetState { id }).await?;
        match self
            .read_until(
                Duration::from_secs(5),
                |event| matches!(event, ServerEvent::State { id: event_id, .. } if *event_id == id),
            )
            .await?
        {
            ServerEvent::State { session_id, .. } => Ok(session_id),
            other => anyhow::bail!("unexpected state response: {other:?}"),
        }
    }

    async fn send_message(&mut self, content: &str) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_request(Request::Message {
            id,
            content: content.to_string(),
            images: Vec::new(),
            system_reminder: None,
            no_reply: false,
        })
        .await
    }

    async fn history(&mut self) -> Result<Vec<HistoryMessage>> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_request(Request::GetHistory { id }).await?;
        match self
            .read_until(Duration::from_secs(5), |event| {
                matches!(event, ServerEvent::History { id: event_id, .. } if *event_id == id)
            })
            .await?
        {
            ServerEvent::History { messages, .. } => Ok(messages),
            other => anyhow::bail!("unexpected history response: {other:?}"),
        }
    }
}

#[derive(Default)]
struct RoundTripProviderState {
    eve_turns: AtomicUsize,
    atlas_turns: AtomicUsize,
    atlas_reply_calls: AtomicUsize,
    atlas_prompt: StdMutex<Option<String>>,
    atlas_reply_recorded: Notify,
    release_atlas: Notify,
}

#[derive(Clone)]
struct RoundTripProvider {
    state: Arc<RoundTripProviderState>,
}

fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } | ContentBlock::ToolResult { content: text, .. } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn provider_stream(events: Vec<StreamEvent>) -> EventStream {
    Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
}

fn tool_call_stream(id: &str, input: serde_json::Value) -> EventStream {
    provider_stream(vec![
        StreamEvent::ToolUseStart {
            id: id.to_string(),
            name: "peer".to_string(),
        },
        StreamEvent::ToolInputDelta(input.to_string()),
        StreamEvent::ToolUseEnd,
        StreamEvent::MessageEnd {
            stop_reason: Some("tool_use".to_string()),
        },
    ])
}

fn text_stream(text: &str) -> EventStream {
    provider_stream(vec![
        StreamEvent::TextDelta(text.to_string()),
        StreamEvent::MessageEnd {
            stop_reason: Some("end_turn".to_string()),
        },
    ])
}

#[async_trait]
impl Provider for RoundTripProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        if !tools.iter().any(|tool| tool.name == "peer") {
            anyhow::bail!("peer tool was not exposed to the scripted provider")
        }
        let previews = messages.iter().map(message_text).collect::<Vec<_>>();
        let transcript = previews.join("\n");

        if transcript.contains("Verified peer message from Eve (`eve-project`)") {
            if transcript.contains("Peer reply recorded for `peer_") {
                self.state.atlas_reply_recorded.notify_one();
                self.state.release_atlas.notified().await;
                return Ok(text_stream("Atlas completed the bounded peer turn."));
            }

            self.state.atlas_turns.fetch_add(1, Ordering::SeqCst);
            self.state.atlas_reply_calls.fetch_add(1, Ordering::SeqCst);
            let prompt = previews
                .iter()
                .find(|preview| preview.contains("Verified peer message from Eve"))
                .cloned()
                .expect("Atlas provider should receive the verified peer prompt");
            *self.state.atlas_prompt.lock().unwrap() = Some(prompt);
            return Ok(tool_call_stream(
                "atlas-peer-reply",
                serde_json::json!({
                    "action": "reply",
                    "message": "Atlas reviewed the request successfully."
                }),
            ));
        }

        if transcript.contains("Start the deterministic peer round trip.") {
            if transcript.contains("Atlas reviewed the request successfully.") {
                return Ok(text_stream("Eve received Atlas's peer reply."));
            }

            self.state.eve_turns.fetch_add(1, Ordering::SeqCst);
            return Ok(tool_call_stream(
                "eve-peer-send",
                serde_json::json!({
                    "action": "send",
                    "to": "Atlas",
                    "message": "Please review the deterministic round trip."
                }),
            ));
        }

        anyhow::bail!("unexpected scripted provider transcript: {transcript}")
    }

    fn name(&self) -> &str {
        "peer-round-trip"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

async fn wait_for_server_socket(
    path: &Path,
    server_task: &mut tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if server_task.is_finished() {
            let result = server_task.await?;
            return Err(anyhow::anyhow!(
                "server exited before socket became ready: {result:?}"
            ));
        }
        match Stream::connect(path).await {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_handler_completes_one_scripted_peer_round_trip_and_releases_both_sessions() {
    let _env_lock = crate::storage::lock_test_env();
    let root = tempfile::TempDir::new().expect("peer round-trip root");
    let home = root.path().join("home");
    let runtime = root.path().join("runtime");
    let eve_dir = root.path().join("eve-project");
    let atlas_dir = root.path().join("atlas-project");
    for directory in [&home, &runtime, &eve_dir, &atlas_dir] {
        std::fs::create_dir_all(directory).expect("create peer test directory");
    }
    let peer_config = serde_json::json!({
        "version": 1,
        "groups": [{
            "name": "reviewers",
            "members": [
                { "alias": "Eve", "working_dir": eve_dir },
                { "alias": "Atlas", "working_dir": atlas_dir }
            ]
        }]
    });
    std::fs::write(
        home.join("peer-groups.json"),
        serde_json::to_vec(&peer_config).expect("serialize peer config"),
    )
    .expect("write peer config");

    let socket_path = runtime.join("jcode.sock");
    let debug_socket_path = runtime.join("jcode-debug.sock");
    let _home = EnvGuard::set("JCODE_HOME", &home);
    let _runtime = EnvGuard::set("JCODE_RUNTIME_DIR", &runtime);
    let _socket = EnvGuard::set("JCODE_SOCKET", &socket_path);
    let _peer_enabled = EnvGuard::set("JCODE_PEER_MESSAGING_ENABLED", "1");
    assert!(crate::config::config().features.peer_messaging);

    let state = Arc::new(RoundTripProviderState::default());
    let provider: Arc<dyn Provider> = Arc::new(RoundTripProvider {
        state: Arc::clone(&state),
    });
    let server = Arc::new(Server::new_with_paths(
        provider,
        socket_path.clone(),
        debug_socket_path,
    ));
    let mut server_task = {
        let server = Arc::clone(&server);
        tokio::spawn(async move { server.run().await })
    };
    wait_for_server_socket(&socket_path, &mut server_task)
        .await
        .expect("server socket should be ready");

    let mut eve = RawClient::connect(&socket_path)
        .await
        .expect("Eve should connect");
    let mut atlas = RawClient::connect(&socket_path)
        .await
        .expect("Atlas should connect");
    eve.subscribe(&eve_dir).await.expect("Eve subscribe");
    atlas.subscribe(&atlas_dir).await.expect("Atlas subscribe");
    let eve_session = eve.session_id().await.expect("Eve session id");
    let atlas_session = atlas.session_id().await.expect("Atlas session id");
    assert_ne!(eve_session, atlas_session);

    let eve_request_id = eve
        .send_message("Start the deterministic peer round trip.")
        .await
        .expect("Eve normal turn should start");
    tokio::time::timeout(
        Duration::from_secs(10),
        state.atlas_reply_recorded.notified(),
    )
    .await
    .expect("Atlas should record its scripted reply");

    let atlas_context = server
        .turn_coordinator
        .active_context_for_test(&atlas_session)
        .expect("Atlas peer-inbound lease should still be active");
    assert!(matches!(
        atlas_context.origin,
        TurnOrigin::PeerInbound { .. }
    ));
    assert_eq!(
        server.turn_coordinator.consume_send_permit(&atlas_context),
        Err(TurnValidationError::OriginNotAllowed),
        "Atlas's PeerInbound lease must have no send permit"
    );

    let caller = PeerCaller {
        session_id: atlas_session.clone(),
        generation: atlas_context.turn_generation.expect("Atlas generation"),
        capability: atlas_context
            .turn_capability
            .as_ref()
            .expect("Atlas capability")
            .expose_secret()
            .to_string(),
    };
    let mut duplicate_reply = RawClient::connect(&socket_path)
        .await
        .expect("duplicate reply client should connect");
    duplicate_reply
        .send_request(Request::PeerReply {
            id: 91,
            caller,
            message: "A second reply must be rejected.".to_string(),
        })
        .await
        .expect("duplicate reply request should reach the server");
    let duplicate_event = duplicate_reply
        .read_until(Duration::from_secs(5), |event| {
            matches!(event, ServerEvent::Error { id: 91, .. })
        })
        .await
        .expect("duplicate reply should receive an error");
    assert!(matches!(
        duplicate_event,
        ServerEvent::Error { message, .. }
            if message == "This peer message has already been replied to."
    ));

    state.release_atlas.notify_one();

    let eve_peer_result = eve
        .read_until(Duration::from_secs(10), |event| {
            matches!(
                event,
                ServerEvent::ToolDone { name, error: None, .. } if name == "peer"
            )
        })
        .await
        .expect("Eve's waiting peer send should return");
    let ServerEvent::ToolDone { output, .. } = eve_peer_result else {
        unreachable!("peer result predicate only accepts ToolDone")
    };
    assert!(output.contains("Peer reply from Atlas (`atlas-project`)."));
    assert!(output.contains("Atlas reviewed the request successfully."));
    eve.read_until(
        Duration::from_secs(10),
        |event| matches!(event, ServerEvent::Done { id } if *id == eve_request_id),
    )
    .await
    .expect("Eve turn should complete");
    atlas
        .read_until(Duration::from_secs(10), |event| {
            matches!(event, ServerEvent::Done { id: 0 })
        })
        .await
        .expect("Atlas recipient turn should complete");

    let history = atlas.history().await.expect("Atlas history");
    let peer_history = history
        .iter()
        .filter(|message| message.role == "peer")
        .collect::<Vec<_>>();
    assert_eq!(peer_history.len(), 1, "exactly one inbound peer message");
    assert!(peer_history[0].content.contains("Eve (`eve-project`)"));

    let persisted = Session::load(&atlas_session).expect("load persisted Atlas session");
    let stored_peer_messages = persisted
        .messages
        .iter()
        .filter(|message| message.display_role == Some(StoredDisplayRole::Peer))
        .collect::<Vec<_>>();
    assert_eq!(stored_peer_messages.len(), 1);
    let stored_peer_body = stored_peer_messages[0].content_preview();
    assert!(stored_peer_body.contains("Verified peer message from Eve (`eve-project`)"));
    assert!(stored_peer_body.contains("to Atlas (`atlas-project`)"));
    assert!(stored_peer_body.contains("Please review the deterministic round trip."));

    assert_eq!(state.eve_turns.load(Ordering::SeqCst), 1);
    assert_eq!(state.atlas_turns.load(Ordering::SeqCst), 1);
    assert_eq!(state.atlas_reply_calls.load(Ordering::SeqCst), 1);
    let provider_prompt = state
        .atlas_prompt
        .lock()
        .unwrap()
        .clone()
        .expect("captured Atlas provider prompt");
    assert!(provider_prompt.contains("Verified peer message from Eve (`eve-project`)"));
    assert!(provider_prompt.contains("to Atlas (`atlas-project`)"));
    assert!(provider_prompt.contains("Please review the deterministic round trip."));

    assert_eq!(server.peer_exchanges.active_exchange_count(), 0);
    assert!(!server.turn_coordinator.session_is_busy(&eve_session));
    assert!(!server.turn_coordinator.session_is_busy(&atlas_session));
    let eve_probe = server
        .turn_coordinator
        .begin_server_turn(&eve_session, TurnOrigin::NormalUser)
        .expect("Eve reservation should be released");
    let atlas_probe = server
        .turn_coordinator
        .begin_server_turn(&atlas_session, TurnOrigin::NormalUser)
        .expect("Atlas reservation should be released");
    drop((eve_probe, atlas_probe));

    server_task.abort();
    let _ = server_task.await;
}
