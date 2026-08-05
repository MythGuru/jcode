use super::{
    apply_or_defer_subscribe_working_dir, claim_live_target_agent, effective_subscribe_working_dir,
    handle_clear_session, handle_reload, handle_resume_session, handle_subscribe,
    mark_remote_reload_started, remove_detached_source_if_unclaimed, rename_shutdown_signal,
    rename_swarm_member_session, restored_session_was_interrupted,
    session_was_interrupted_by_reload, subscribe_should_mark_ready,
    subscribe_working_dir_replacement,
};
use crate::agent::Agent;
use crate::message::ContentBlock;
use crate::message::{Message, ToolDefinition};
use crate::protocol::ServerEvent;
use crate::provider::{EventStream, Provider};
use crate::server::{
    ClientConnectionInfo, ClientDebugState, FileTouchService, SessionInterruptQueues, SwarmEvent,
    SwarmMember, VersionedPlan,
};
use crate::tool::Registry;
use anyhow::Result;
use async_trait::async_trait;
use jcode_agent_runtime::InterruptSignal;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

struct MockProvider;

fn test_swarm_member(session_id: &str, status: &str) -> SwarmMember {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    SwarmMember {
        session_id: session_id.to_string(),
        event_tx,
        event_txs: HashMap::new(),
        working_dir: None,
        swarm_id: Some("swarm-test".to_string()),
        swarm_enabled: true,
        status: status.to_string(),
        detail: None,
        task_label: None,
        friendly_name: Some(session_id.to_string()),
        report_back_to_session_id: Some("coord".to_string()),
        latest_completion_report: None,
        role: "agent".to_string(),
        joined_at: Instant::now(),
        last_status_change: Instant::now(),
        is_headless: false,
        output_tail: None,
        todo_progress: None,
        todo_items: Vec::new(),
        runtime: crate::protocol::SwarmMemberRuntime::default(),
    }
}

#[tokio::test]
async fn subscribe_does_not_mark_running_startup_worker_ready() {
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        "worker".to_string(),
        test_swarm_member("worker", "running"),
    )])));
    assert!(!subscribe_should_mark_ready("worker", &swarm_members).await);
}

#[tokio::test]
async fn subscribe_marks_non_running_member_ready() {
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        "worker".to_string(),
        test_swarm_member("worker", "spawned"),
    )])));
    assert!(subscribe_should_mark_ready("worker", &swarm_members).await);
}

#[tokio::test]
async fn resume_rename_releases_member_lock_before_waiting_for_swarm_map() {
    let old_session_id = "session-old";
    let new_session_id = "session-new";
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (
            old_session_id.to_string(),
            test_swarm_member(old_session_id, "spawned"),
        ),
        (
            "child".to_string(),
            SwarmMember {
                report_back_to_session_id: Some(old_session_id.to_string()),
                ..test_swarm_member("child", "running")
            },
        ),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        "swarm-test".to_string(),
        HashSet::from([old_session_id.to_string(), "child".to_string()]),
    )])));

    // Force the rename to wait for swarms_by_id. While it waits, the member map
    // must remain readable or coordinator cleanup can form a permanent cycle.
    let swarm_map_guard = swarms_by_id.write().await;
    let rename_task = tokio::spawn({
        let swarm_members = Arc::clone(&swarm_members);
        let swarms_by_id = Arc::clone(&swarms_by_id);
        async move {
            rename_swarm_member_session(
                old_session_id,
                new_session_id,
                &swarm_members,
                &swarms_by_id,
            )
            .await;
        }
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let members = swarm_members.read().await;
            if members.contains_key(new_session_id) {
                assert_eq!(
                    members
                        .get("child")
                        .and_then(|member| member.report_back_to_session_id.as_deref()),
                    Some(new_session_id)
                );
                break;
            }
            drop(members);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("member map stayed locked while waiting for swarm map");

    drop(swarm_map_guard);
    rename_task.await.expect("rename task");
    let swarms = swarms_by_id.read().await;
    let swarm = swarms.get("swarm-test").expect("swarm remains present");
    assert!(!swarm.contains(old_session_id));
    assert!(swarm.contains(new_session_id));
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Err(anyhow::anyhow!(
            "mock provider complete should not be called in client_session tests"
        ))
    }

    fn name(&self) -> &str {
        "mock"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(MockProvider)
    }
}

fn test_agent(messages: Vec<crate::session::StoredMessage>) -> Agent {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let _guard = rt.enter();
    let registry = rt.block_on(Registry::new(provider.clone()));
    build_test_agent(provider, registry, messages)
}

fn build_test_agent(
    provider: Arc<dyn Provider>,
    registry: Registry,
    messages: Vec<crate::session::StoredMessage>,
) -> Agent {
    let mut session =
        crate::session::Session::create_with_id("session_test_reload".to_string(), None, None);
    session.model = Some("mock".to_string());
    session.replace_messages(messages);
    Agent::new_with_session(provider, registry, session, None)
}

fn build_test_agent_with_id(
    provider: Arc<dyn Provider>,
    registry: Registry,
    session_id: &str,
    messages: Vec<crate::session::StoredMessage>,
) -> Agent {
    let mut session = crate::session::Session::create_with_id(session_id.to_string(), None, None);
    session.model = Some("mock".to_string());
    session.replace_messages(messages);
    Agent::new_with_session(provider, registry, session, None)
}

async fn collect_events_until_done(
    client_event_rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    done_id: u64,
) -> Vec<ServerEvent> {
    let mut events = Vec::new();
    for _ in 0..16 {
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), client_event_rx.recv())
            .await
            .expect("timed out waiting for server event")
            .expect("expected server event");
        let is_done = matches!(event, ServerEvent::Done { id } if id == done_id);
        events.push(event);
        if is_done {
            break;
        }
    }
    events
}

#[tokio::test]
async fn live_target_claim_is_atomic_with_detached_source_cleanup() {
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;

    for iteration in 0..32 {
        let target_id = format!("session_atomic_target_{iteration}");
        let source_id = format!("session_atomic_source_{iteration}");
        let target_agent = Arc::new(Mutex::new(build_test_agent_with_id(
            provider.clone(),
            registry.clone(),
            &target_id,
            Vec::new(),
        )));
        let source_agent = Arc::new(Mutex::new(build_test_agent_with_id(
            provider.clone(),
            registry.clone(),
            &source_id,
            Vec::new(),
        )));
        let sessions = Arc::new(RwLock::new(HashMap::from([(
            target_id.clone(),
            Arc::clone(&target_agent),
        )])));
        let now = Instant::now();
        let (disconnect_tx, _disconnect_rx) = mpsc::unbounded_channel();
        let connections = Arc::new(RwLock::new(HashMap::from([(
            "incoming".to_string(),
            ClientConnectionInfo {
                client_id: "incoming".to_string(),
                session_id: source_id,
                client_instance_id: None,
                debug_client_id: None,
                connected_at: now,
                last_seen: now,
                is_processing: false,
                current_tool_name: None,
                terminal_env: Vec::new(),
                disconnect_tx,
            },
        )])));
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let claim = {
            let barrier = Arc::clone(&barrier);
            let sessions = Arc::clone(&sessions);
            let connections = Arc::clone(&connections);
            let source_agent = Arc::clone(&source_agent);
            let target_id = target_id.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                claim_live_target_agent(
                    &target_id,
                    "incoming",
                    Some("instance-a"),
                    &source_agent,
                    &sessions,
                    &connections,
                )
                .await
                .is_some()
            })
        };
        let cleanup = {
            let barrier = Arc::clone(&barrier);
            let sessions = Arc::clone(&sessions);
            let connections = Arc::clone(&connections);
            let target_agent = Arc::clone(&target_agent);
            let target_id = target_id.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                remove_detached_source_if_unclaimed(
                    &target_id,
                    "cleanup",
                    &target_agent,
                    &sessions,
                    &connections,
                )
                .await
            })
        };

        barrier.wait().await;
        let claimed = claim.await.expect("claim task should complete");
        let removed = cleanup.await.expect("cleanup task should complete");
        assert_ne!(claimed, removed, "exactly one transition must win");
        assert_eq!(
            sessions.read().await.contains_key(&target_id),
            claimed,
            "a successful claim must keep its target registered"
        );
        if claimed {
            let connections = connections.read().await;
            let incoming = connections.get("incoming").expect("incoming connection");
            assert_eq!(incoming.session_id, target_id);
            assert_eq!(incoming.client_instance_id.as_deref(), Some("instance-a"));
        }
    }
}

/// Issue #481: a subscribe cwd that is merely absolute is not enough. A client
/// reporting the *home* directory must not silently re-pin (or clobber) a
/// session that is already bound to a real project directory, because tools then
/// run in home while the UI still shows the project.
#[test]
fn subscribe_working_dir_ignores_home_when_session_has_a_project_dir() {
    let home = std::path::Path::new("/home/tester");
    let project = "/home/tester/work/project";

    assert_eq!(
        subscribe_working_dir_replacement(Some(project), "/home/tester", Some(home)),
        None,
        "home must not clobber an established project cwd"
    );

    // A session with no cwd yet, or one already in home, may legitimately use home.
    assert_eq!(
        subscribe_working_dir_replacement(None, "/home/tester", Some(home)),
        Some("/home/tester".to_string())
    );
    assert_eq!(
        subscribe_working_dir_replacement(Some("/home/tester"), "/home/tester", Some(home)),
        None,
        "an unchanged cwd needs no reassignment"
    );

    // Genuine project-to-project moves still apply.
    assert_eq!(
        subscribe_working_dir_replacement(Some(project), "/home/tester/work/other", Some(home)),
        Some("/home/tester/work/other".to_string())
    );

    // A subdirectory of home that is not home itself is a real project path.
    assert_eq!(
        subscribe_working_dir_replacement(Some(project), "/home/tester/scratch", Some(home)),
        Some("/home/tester/scratch".to_string())
    );

    // Blank/whitespace reports are never applied, and an unknown home disables
    // the guard rather than rejecting valid directories.
    assert_eq!(
        subscribe_working_dir_replacement(Some(project), "   ", Some(home)),
        None
    );
    assert_eq!(
        subscribe_working_dir_replacement(Some(project), "/home/tester", None),
        Some("/home/tester".to_string())
    );
}

/// Issue #481: agent cwd, swarm grouping, and project-local MCP resolution must
/// all bind to the *same* directory. If a rejected home-dir report still reached
/// the swarm id or the MCP resolver, tools would run in the project while swarm
/// membership and `.jcode/mcp.json` discovery pointed at home.
#[test]
fn effective_subscribe_working_dir_binds_all_consumers_to_one_directory() {
    let home = std::path::Path::new("/home/tester");
    let project = "/home/tester/work/project";

    // Rejected home report: every consumer keeps the project dir.
    assert_eq!(
        effective_subscribe_working_dir(Some(project), "/home/tester", Some(home)),
        project
    );

    // Accepted move: every consumer follows to the new dir.
    assert_eq!(
        effective_subscribe_working_dir(Some(project), "/home/tester/work/other", Some(home)),
        "/home/tester/work/other"
    );

    // No prior cwd: the report is authoritative, including home.
    assert_eq!(
        effective_subscribe_working_dir(None, "/home/tester", Some(home)),
        "/home/tester"
    );

    // Unchanged report resolves to the same dir rather than dropping it.
    assert_eq!(
        effective_subscribe_working_dir(Some(project), project, Some(home)),
        project
    );
}

/// Issue #481 end to end: drive the real subscribe-cwd application path against
/// a live `Agent` and assert the agent's stored working directory, which is what
/// bash/file tools actually run in. The pure-resolver tests above cover the
/// decision; this covers the wiring that applies it.
#[tokio::test]
async fn apply_subscribe_working_dir_keeps_project_when_client_reports_home() {
    let home = dirs::home_dir().expect("home directory");
    let home_str = home.to_string_lossy().to_string();
    let project = home.join("jcode-481-project");
    let project_str = project.to_string_lossy().to_string();

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(Arc::clone(&provider)).await;
    let agent = Arc::new(Mutex::new(Agent::new_with_initial_working_dir(
        provider,
        registry,
        Some(&project_str),
    )));
    let peer_home = tempfile::TempDir::new().expect("empty peer config home");
    let peer_groups = jcode_base::peer_groups::PeerGroups::load_from_jcode_home(peer_home.path())
        .expect("load empty peer groups");
    let peer_exchanges = crate::server::peer_exchange::PeerExchangeRegistry::new(
        crate::server::turn_coordinator::TurnCoordinator::default(),
        std::time::Duration::from_secs(60),
    );

    assert_eq!(
        agent.lock().await.working_dir(),
        Some(project_str.as_str()),
        "precondition: session starts bound to the project"
    );

    // A client whose inherited cwd is home must not re-pin the session.
    apply_or_defer_subscribe_working_dir(
        &agent,
        &home_str,
        "session_test_481",
        &peer_groups,
        &peer_exchanges,
    );
    assert_eq!(
        agent.lock().await.working_dir(),
        Some(project_str.as_str()),
        "a home-dir subscribe must not clobber the project cwd"
    );

    // A genuine project-to-project move still applies.
    let other = home.join("jcode-481-other");
    let other_str = other.to_string_lossy().to_string();
    apply_or_defer_subscribe_working_dir(
        &agent,
        &other_str,
        "session_test_481",
        &peer_groups,
        &peer_exchanges,
    );
    assert_eq!(
        agent.lock().await.working_dir(),
        Some(other_str.as_str()),
        "a real directory change must still be honored"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_directory_change_invalidates_peer_identity_before_async_bookkeeping() {
    let home = tempfile::TempDir::new().expect("peer config home");
    let eve_dir = tempfile::TempDir::new().expect("Eve project");
    let atlas_dir = tempfile::TempDir::new().expect("Atlas project");
    let moved_dir = tempfile::TempDir::new().expect("moved project");
    let config = serde_json::json!({
        "version": 1,
        "groups": [{
            "name": "reviewers",
            "members": [
                { "alias": "Eve", "working_dir": eve_dir.path() },
                { "alias": "Atlas", "working_dir": atlas_dir.path() }
            ]
        }]
    });
    std::fs::write(
        home.path().join("peer-groups.json"),
        serde_json::to_vec(&config).expect("serialize peer config"),
    )
    .expect("write peer config");
    let peer_groups = jcode_base::peer_groups::PeerGroups::load_from_jcode_home(home.path())
        .expect("load peer groups");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let sender_id = "subscribe-peer-sender";
    let recipient_id = "subscribe-peer-recipient";
    let sender_agent = Arc::new(Mutex::new(build_test_agent_with_id(
        provider.clone(),
        registry.clone(),
        sender_id,
        Vec::new(),
    )));
    let recipient_agent = Arc::new(Mutex::new(build_test_agent_with_id(
        provider,
        registry.clone(),
        recipient_id,
        Vec::new(),
    )));
    let sessions = Arc::new(RwLock::new(HashMap::from([
        (sender_id.to_string(), sender_agent),
        (recipient_id.to_string(), Arc::clone(&recipient_agent)),
    ])));

    let mut sender_member = test_swarm_member(sender_id, "ready");
    sender_member.working_dir = Some(eve_dir.path().to_path_buf());
    let mut recipient_member = test_swarm_member(recipient_id, "ready");
    recipient_member.working_dir = Some(atlas_dir.path().to_path_buf());
    recipient_member.swarm_id = Some("old-atlas-swarm".to_string());
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (sender_id.to_string(), sender_member),
        (recipient_id.to_string(), recipient_member),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        "old-atlas-swarm".to_string(),
        HashSet::from([recipient_id.to_string()]),
    )])));
    let channel_subscriptions = Arc::new(RwLock::new(HashMap::<
        String,
        HashMap<String, HashSet<String>>,
    >::new()));
    let channel_subscriptions_by_session = Arc::new(RwLock::new(HashMap::<
        String,
        HashMap<String, HashSet<String>>,
    >::new()));
    let swarm_plans = Arc::new(RwLock::new(HashMap::<String, VersionedPlan>::new()));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::<String, String>::new()));
    let (client_event_tx, _client_event_rx) = mpsc::unbounded_channel::<ServerEvent>();
    let event_history = Arc::new(RwLock::new(VecDeque::<SwarmEvent>::new()));
    let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel::<SwarmEvent>(8);
    let mcp_pool = Arc::new(crate::mcp::SharedMcpPool::from_default_config());
    let coordinator = crate::server::turn_coordinator::TurnCoordinator::default();
    let peer_exchanges = crate::server::peer_exchange::PeerExchangeRegistry::new(
        coordinator.clone(),
        std::time::Duration::from_secs(60),
    );
    peer_exchanges.pin_or_invalidate_session(sender_id, eve_dir.path(), &peer_groups);
    peer_exchanges.pin_or_invalidate_session(recipient_id, atlas_dir.path(), &peer_groups);
    let sender_lease = coordinator
        .begin_server_turn(sender_id, crate::tool::TurnOrigin::NormalUser)
        .expect("sender turn");
    let sender_context = sender_lease.context().clone();

    // Block the first swarms_by_id update after the member directory changes.
    // This exposes the old await gap deterministically without test-only hooks.
    let swarms_blocker = swarms_by_id.write().await;
    let subscribe_task = tokio::spawn({
        let recipient_agent = Arc::clone(&recipient_agent);
        let registry = registry.clone();
        let swarm_members = Arc::clone(&swarm_members);
        let swarms_by_id = Arc::clone(&swarms_by_id);
        let channel_subscriptions = Arc::clone(&channel_subscriptions);
        let channel_subscriptions_by_session = Arc::clone(&channel_subscriptions_by_session);
        let swarm_plans = Arc::clone(&swarm_plans);
        let swarm_coordinators = Arc::clone(&swarm_coordinators);
        let client_event_tx = client_event_tx.clone();
        let mcp_pool = Arc::clone(&mcp_pool);
        let event_history = Arc::clone(&event_history);
        let event_counter = Arc::clone(&event_counter);
        let swarm_event_tx = swarm_event_tx.clone();
        let peer_groups = peer_groups.clone();
        let peer_exchanges = peer_exchanges.clone();
        let moved_dir = moved_dir.path().to_string_lossy().to_string();
        async move {
            let mut client_selfdev = false;
            handle_subscribe(
                91,
                Some(moved_dir),
                None,
                false,
                &mut client_selfdev,
                recipient_id,
                "subscribe-peer-connection",
                &None,
                &recipient_agent,
                &registry,
                true,
                &swarm_members,
                &swarms_by_id,
                &channel_subscriptions,
                &channel_subscriptions_by_session,
                &swarm_plans,
                &swarm_coordinators,
                &client_event_tx,
                &mcp_pool,
                &event_history,
                &event_counter,
                &swarm_event_tx,
                &peer_groups,
                &peer_exchanges,
            )
            .await;
        }
    });

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let moved = swarm_members
                .read()
                .await
                .get(recipient_id)
                .and_then(|member| member.working_dir.as_ref())
                .is_some_and(|path| path == moved_dir.path());
            if moved {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("subscribe reached the async bookkeeping gap");

    let stale_start = {
        let sessions = sessions.read().await;
        let members = swarm_members.read().await;
        peer_exchanges.resolve_and_register(
            &sender_context,
            "Atlas",
            &peer_groups,
            sessions.keys().map(String::as_str),
            |session_id| {
                members.get(session_id).is_some_and(|member| {
                    !member.event_txs.is_empty() || !member.event_tx.is_closed()
                })
            },
        )
    };
    match stale_start {
        Err(crate::server::peer_exchange::PeerStartError::TargetOffline(alias)) => {
            assert_eq!(alias, "Atlas")
        }
        Err(other) => panic!("expected stale Atlas identity to be ineligible, got {other:?}"),
        Ok(start) => {
            let exchange_id = start.exchange_id.clone();
            drop(start);
            let _ = peer_exchanges.cancel_exchange(&exchange_id);
            panic!("peer start authorized Atlas after its effective directory changed");
        }
    }

    drop(swarms_blocker);
    subscribe_task.await.expect("subscribe task");
    drop(sender_lease);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_unpinned_subscribe_refuses_reported_peer_identity_until_agent_moves() {
    let home = tempfile::TempDir::new().expect("peer config home");
    let eve_dir = tempfile::TempDir::new().expect("Eve project");
    let atlas_dir = tempfile::TempDir::new().expect("Atlas project");
    let config = serde_json::json!({
        "version": 1,
        "groups": [{
            "name": "reviewers",
            "members": [
                { "alias": "Eve", "working_dir": eve_dir.path() },
                { "alias": "Atlas", "working_dir": atlas_dir.path() }
            ]
        }]
    });
    std::fs::write(
        home.path().join("peer-groups.json"),
        serde_json::to_vec(&config).expect("serialize peer config"),
    )
    .expect("write peer config");
    let peer_groups = jcode_base::peer_groups::PeerGroups::load_from_jcode_home(home.path())
        .expect("load peer groups");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let sender_id = "busy-unpinned-sender";
    let recipient_id = "busy-unpinned-recipient";
    let sender_agent = Arc::new(Mutex::new(build_test_agent_with_id(
        provider.clone(),
        registry.clone(),
        sender_id,
        Vec::new(),
    )));
    let recipient_agent = Arc::new(Mutex::new(build_test_agent_with_id(
        provider,
        registry.clone(),
        recipient_id,
        Vec::new(),
    )));
    sender_agent
        .lock()
        .await
        .set_working_dir(&eve_dir.path().to_string_lossy());
    recipient_agent
        .lock()
        .await
        .set_working_dir(&eve_dir.path().to_string_lossy());
    let sessions = Arc::new(RwLock::new(HashMap::from([
        (sender_id.to_string(), sender_agent),
        (recipient_id.to_string(), Arc::clone(&recipient_agent)),
    ])));

    let mut sender_member = test_swarm_member(sender_id, "ready");
    sender_member.working_dir = Some(eve_dir.path().to_path_buf());
    let mut recipient_member = test_swarm_member(recipient_id, "running");
    recipient_member.working_dir = Some(eve_dir.path().to_path_buf());
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (sender_id.to_string(), sender_member),
        (recipient_id.to_string(), recipient_member),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        "swarm-test".to_string(),
        HashSet::from([sender_id.to_string(), recipient_id.to_string()]),
    )])));
    let channel_subscriptions = Arc::new(RwLock::new(HashMap::<
        String,
        HashMap<String, HashSet<String>>,
    >::new()));
    let channel_subscriptions_by_session = Arc::new(RwLock::new(HashMap::<
        String,
        HashMap<String, HashSet<String>>,
    >::new()));
    let swarm_plans = Arc::new(RwLock::new(HashMap::<String, VersionedPlan>::new()));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::<String, String>::new()));
    let (client_event_tx, _client_event_rx) = mpsc::unbounded_channel::<ServerEvent>();
    let event_history = Arc::new(RwLock::new(VecDeque::<SwarmEvent>::new()));
    let event_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel::<SwarmEvent>(8);
    let mcp_pool = Arc::new(crate::mcp::SharedMcpPool::from_default_config());
    let coordinator = crate::server::turn_coordinator::TurnCoordinator::default();
    let peer_exchanges = crate::server::peer_exchange::PeerExchangeRegistry::new(
        coordinator.clone(),
        std::time::Duration::from_secs(60),
    );
    peer_exchanges.pin_or_invalidate_session(sender_id, eve_dir.path(), &peer_groups);
    let sender_lease = coordinator
        .begin_server_turn(sender_id, crate::tool::TurnOrigin::NormalUser)
        .expect("sender turn");
    let sender_context = sender_lease.context().clone();

    // Holding the Agent lock forces subscribe's working-directory update into
    // the deferred task. Until that task can move the Agent from Eve to Atlas,
    // the session must remain ineligible as Atlas.
    let recipient_guard = recipient_agent.lock().await;
    let mut client_selfdev = false;
    handle_subscribe(
        92,
        Some(atlas_dir.path().to_string_lossy().to_string()),
        None,
        false,
        &mut client_selfdev,
        recipient_id,
        "busy-unpinned-connection",
        &None,
        &recipient_agent,
        &registry,
        true,
        &swarm_members,
        &swarms_by_id,
        &channel_subscriptions,
        &channel_subscriptions_by_session,
        &swarm_plans,
        &swarm_coordinators,
        &client_event_tx,
        &mcp_pool,
        &event_history,
        &event_counter,
        &swarm_event_tx,
        &peer_groups,
        &peer_exchanges,
    )
    .await;

    let premature_start = {
        let sessions = sessions.read().await;
        let members = swarm_members.read().await;
        peer_exchanges.resolve_and_register(
            &sender_context,
            "Atlas",
            &peer_groups,
            sessions.keys().map(String::as_str),
            |session_id| {
                members.get(session_id).is_some_and(|member| {
                    !member.event_txs.is_empty() || !member.event_tx.is_closed()
                })
            },
        )
    };
    match premature_start {
        Err(crate::server::peer_exchange::PeerStartError::TargetOffline(alias)) => {
            assert_eq!(alias, "Atlas")
        }
        Err(other) => panic!("expected premature Atlas identity to be refused, got {other:?}"),
        Ok(start) => {
            let exchange_id = start.exchange_id.clone();
            drop(start);
            let _ = peer_exchanges.cancel_exchange(&exchange_id);
            panic!("peer start authorized Atlas while its Agent still worked in Eve");
        }
    }

    drop(recipient_guard);
    drop(sender_lease);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_subscribe_cannot_change_a_replacement_sessions_working_directory() {
    let home = tempfile::TempDir::new().expect("peer config home");
    let eve_dir = tempfile::TempDir::new().expect("Eve project");
    let atlas_dir = tempfile::TempDir::new().expect("Atlas project");
    let stale_deferred_dir = tempfile::TempDir::new().expect("stale deferred project");
    let config = serde_json::json!({
        "version": 1,
        "groups": [{
            "name": "reviewers",
            "members": [
                { "alias": "Eve", "working_dir": eve_dir.path() },
                { "alias": "Atlas", "working_dir": atlas_dir.path() }
            ]
        }]
    });
    std::fs::write(
        home.path().join("peer-groups.json"),
        serde_json::to_vec(&config).expect("serialize peer config"),
    )
    .expect("write peer config");
    let peer_groups = jcode_base::peer_groups::PeerGroups::load_from_jcode_home(home.path())
        .expect("load peer groups");

    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    let registry = Registry::new(provider.clone()).await;
    let old_session_id = "deferred-old-session";
    let replacement_session_id = "deferred-replacement-session";
    let sender_id = "deferred-sender";
    let recipient_agent = Arc::new(Mutex::new(build_test_agent_with_id(
        provider.clone(),
        registry.clone(),
        old_session_id,
        Vec::new(),
    )));
    let mut recipient_guard = recipient_agent.lock().await;

    let coordinator = crate::server::turn_coordinator::TurnCoordinator::default();
    let peer_exchanges = crate::server::peer_exchange::PeerExchangeRegistry::new(
        coordinator.clone(),
        std::time::Duration::from_secs(60),
    );

    // Force the subscribe update into its deferred task, then replace the Agent
    // stored in the same Arc before that task can acquire the lock. This is what
    // clear/resume can do after a busy subscribe has returned to the request loop.
    apply_or_defer_subscribe_working_dir(
        &recipient_agent,
        &stale_deferred_dir.path().to_string_lossy(),
        old_session_id,
        &peer_groups,
        &peer_exchanges,
    );
    *recipient_guard =
        build_test_agent_with_id(provider, registry, replacement_session_id, Vec::new());
    recipient_guard.set_working_dir(&atlas_dir.path().to_string_lossy());
    peer_exchanges.pin_or_invalidate_session(
        replacement_session_id,
        atlas_dir.path(),
        &peer_groups,
    );

    // Let the spawned task queue behind our guard. Tokio's FIFO mutex then
    // guarantees it runs before the verification lock below.
    tokio::task::yield_now().await;
    drop(recipient_guard);
    let actual_dir = recipient_agent
        .lock()
        .await
        .working_dir()
        .map(str::to_string);
    assert_eq!(
        actual_dir.as_deref(),
        Some(atlas_dir.path().to_string_lossy().as_ref()),
        "a deferred subscribe for the old session changed the replacement session"
    );

    peer_exchanges.pin_or_invalidate_session(sender_id, eve_dir.path(), &peer_groups);
    let sender_lease = coordinator
        .begin_server_turn(sender_id, crate::tool::TurnOrigin::NormalUser)
        .expect("sender turn");
    let authorization = peer_exchanges.resolve_and_register(
        sender_lease.context(),
        "Atlas",
        &peer_groups,
        [sender_id, replacement_session_id],
        |_| true,
    );
    match authorization {
        Err(crate::server::peer_exchange::PeerStartError::TargetOffline(alias)) => {
            assert_eq!(alias, "Atlas")
        }
        Err(other) => panic!("expected replaced Atlas session to be ineligible, got {other:?}"),
        Ok(start) => {
            let exchange_id = start.exchange_id.clone();
            drop(start);
            let _ = peer_exchanges.cancel_exchange(&exchange_id);
            panic!("wrong-identity authorization succeeded after deferred session replacement");
        }
    }
}

#[path = "client_session_tests/clear.rs"]
mod clear_tests;
#[path = "client_session_tests/reload.rs"]
mod reload_tests;
#[path = "client_session_tests/resume.rs"]
mod resume_tests;
