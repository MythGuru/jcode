use super::App;
use crate::bus::{Bus, BusEvent, PeerOverviewCompleted};
use crate::peer_activity::{PeerActivityDirection, PeerActivityOutcome, PeerActivityReport};
use crate::protocol::{PeerOverview, PeerOverviewState, PeerState};
use jcode_tui_messages::DisplayMessage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeersCommandMatch {
    Exact,
    UsageError,
    NotPeers,
}

fn classify_peers_command(trimmed: &str) -> PeersCommandMatch {
    if trimmed == "/peers" {
        PeersCommandMatch::Exact
    } else if trimmed.starts_with("/peers ") {
        PeersCommandMatch::UsageError
    } else {
        PeersCommandMatch::NotPeers
    }
}

fn completion_belongs_to_session(completed_session_id: &str, active_session_id: &str) -> bool {
    completed_session_id == active_session_id
}

fn should_load_history(state: PeerOverviewState) -> bool {
    matches!(
        state,
        PeerOverviewState::Enabled | PeerOverviewState::Unlisted
    )
}

fn should_load_activity(
    state: PeerOverviewState,
    working_dir: Option<&std::path::PathBuf>,
) -> bool {
    should_load_history(state) && working_dir.is_some()
}

fn activity_working_dir(
    is_remote: bool,
    working_dir: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    (!is_remote).then_some(working_dir).flatten()
}

fn render_peer_overview(
    overview: &PeerOverview,
    activity: Option<&PeerActivityReport>,
    ambient_enabled: bool,
) -> String {
    let messaging = match overview.state {
        PeerOverviewState::Enabled | PeerOverviewState::Unlisted => "ON",
        PeerOverviewState::Disabled => "OFF",
        PeerOverviewState::ConfigurationError => "CONFIGURATION ERROR",
    };
    let mut lines = vec![format!("Peer Messaging: {messaging}")];
    match (&overview.state, &overview.identity) {
        (PeerOverviewState::Enabled, Some(identity)) => lines.push(format!(
            "You: {} (`{}`) · group `{}`",
            identity.alias, identity.project, identity.group
        )),
        (PeerOverviewState::Unlisted, _) => {
            lines.push("You: This project is not listed in an approved peer group.".to_string())
        }
        (PeerOverviewState::ConfigurationError, _) => lines.push(
            "Peer configuration needs attention. Check ~/.jcode/peer-groups.json.".to_string(),
        ),
        (PeerOverviewState::Disabled, _) => {
            lines.push("Peer messaging is disabled in Jcode configuration.".to_string())
        }
        _ => lines.push("You: peer identity unavailable.".to_string()),
    }
    lines.push(format!(
        "Ambient initiation: {} (peer initiation unavailable)",
        if ambient_enabled { "ON" } else { "OFF" }
    ));
    lines.push(String::new());
    lines.push("Approved peers".to_string());
    if overview.peers.is_empty() {
        lines.push("No approved peers are available for this project.".to_string());
    } else {
        for peer in &overview.peers {
            let (symbol, state) = match peer.state {
                PeerState::Idle => ("●", "ready"),
                PeerState::Busy => ("◐", "busy"),
                PeerState::Offline => ("○", "offline"),
                PeerState::Ambiguous => ("!", "ambiguous"),
            };
            lines.push(format!(
                "{symbol} {} · {state} · {}",
                peer.alias, peer.project
            ));
        }
    }
    lines.push(String::new());
    lines.push("Recent activity".to_string());
    match activity {
        Some(report) if !report.activities.is_empty() => {
            for item in &report.activities {
                let time = item
                    .occurred_at
                    .map(|time| time.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| "time unavailable".to_string());
                let direction = match item.direction {
                    PeerActivityDirection::Outbound => "→",
                    PeerActivityDirection::Inbound => "←",
                };
                let peer = item
                    .peer_project
                    .as_deref()
                    .map(|project| format!("{} (`{project}`)", item.peer_alias))
                    .unwrap_or_else(|| item.peer_alias.clone());
                let outcome = match item.outcome {
                    PeerActivityOutcome::Sent => "sent",
                    PeerActivityOutcome::Replied => "replied",
                    PeerActivityOutcome::CompletedWithoutReply => "completed without reply",
                    PeerActivityOutcome::Failed => "failed",
                    PeerActivityOutcome::TimedOut => "timed out",
                    PeerActivityOutcome::Cancelled => "cancelled",
                    PeerActivityOutcome::Received => "received",
                    PeerActivityOutcome::OutcomeUnavailable => "outcome unavailable",
                };
                lines.push(format!("{time} {direction} {peer} · {outcome}"));
            }
        }
        Some(_) => lines.push("No saved peer activity yet.".to_string()),
        None => lines.push("Recent activity is unavailable in this state.".to_string()),
    }
    if activity.is_some_and(|report| report.history_limited || report.read_errors > 0) {
        lines.push(String::new());
        lines.push("Some older activity was skipped to keep this view fast.".to_string());
    }
    lines.join("\n")
}

fn load_activity_for_workspace(working_dir: Option<std::path::PathBuf>) -> PeerActivityReport {
    let Some(working_dir) = working_dir else {
        return PeerActivityReport {
            activities: Vec::new(),
            history_limited: true,
            read_errors: 1,
        };
    };
    let canonical = match working_dir.canonicalize() {
        Ok(canonical) => canonical,
        Err(_) => {
            return PeerActivityReport {
                activities: Vec::new(),
                history_limited: true,
                read_errors: 1,
            };
        }
    };
    crate::peer_activity::load_recent_peer_activity(&canonical).unwrap_or(PeerActivityReport {
        activities: Vec::new(),
        history_limited: true,
        read_errors: 1,
    })
}

fn build_peer_overview_card(
    session_id: &str,
    working_dir: Option<std::path::PathBuf>,
    ambient_enabled: bool,
) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Unable to start peer overview: {error}"))?;
    let overview = runtime
        .block_on(crate::peer_overview::fetch_peer_overview(session_id))
        .map_err(|error| error.to_string())?;
    let activity = should_load_activity(overview.state, working_dir.as_ref())
        .then(|| load_activity_for_workspace(working_dir));
    Ok(render_peer_overview(
        &overview,
        activity.as_ref(),
        ambient_enabled,
    ))
}

pub(super) fn handle_peers_command(app: &mut App, trimmed: &str) -> bool {
    match classify_peers_command(trimmed) {
        PeersCommandMatch::UsageError => {
            app.push_display_message(DisplayMessage::error("Usage: /peers".to_string()));
            return true;
        }
        PeersCommandMatch::NotPeers => return false,
        PeersCommandMatch::Exact => {}
    }

    let session_id = super::commands::active_session_id(app);
    let working_dir = activity_working_dir(app.is_remote, super::commands::active_working_dir(app));
    if !app.is_remote
        && app.session.id == session_id
        && let Err(error) = app.session.save()
    {
        app.push_display_message(DisplayMessage::error(format!(
            "Unable to save the current session before reading peer activity: {error}"
        )));
        return true;
    }
    let ambient_enabled = crate::config::config().ambient.enabled;
    app.set_status_notice("Peer overview loading...");
    std::thread::spawn(move || {
        let result = build_peer_overview_card(&session_id, working_dir, ambient_enabled);
        Bus::global().publish(BusEvent::PeerOverviewCompleted(PeerOverviewCompleted {
            session_id,
            result,
        }));
    });
    true
}

pub(super) fn handle_peer_overview_completed(app: &mut App, completed: PeerOverviewCompleted) {
    if !completion_belongs_to_session(
        &completed.session_id,
        &super::commands::active_session_id(app),
    ) {
        return;
    }
    match completed.result {
        Ok(message) => {
            app.push_display_message(DisplayMessage::system(message));
            app.set_status_notice("Peer overview");
        }
        Err(error) => app.push_display_message(DisplayMessage::error(format!(
            "Unable to load peer overview: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_activity::PeerActivity;
    use crate::protocol::{PeerIdentityInfo, PeerInfo, PeerState};
    use chrono::{TimeZone, Utc};

    #[test]
    fn peers_command_accepts_only_the_exact_model_free_command() {
        assert_eq!(classify_peers_command("/peers"), PeersCommandMatch::Exact);
        assert_eq!(
            classify_peers_command("/peers extra"),
            PeersCommandMatch::UsageError
        );
        assert_eq!(
            classify_peers_command("/peers-extra"),
            PeersCommandMatch::NotPeers
        );
        assert_eq!(classify_peers_command("hello"), PeersCommandMatch::NotPeers);
    }

    #[test]
    fn peers_command_completion_is_scoped_to_the_active_session() {
        assert!(completion_belongs_to_session("active", "active"));
        assert!(!completion_belongs_to_session("old", "active"));
    }

    #[test]
    fn remote_peers_command_never_reuses_local_working_directory_for_activity() {
        let stale_local = Some(std::path::PathBuf::from("C:\\stale-local"));
        let working_dir = activity_working_dir(true, stale_local);
        assert_eq!(working_dir, None);
        assert!(!should_load_activity(
            PeerOverviewState::Enabled,
            working_dir.as_ref()
        ));

        let overview = PeerOverview {
            state: PeerOverviewState::Enabled,
            identity: None,
            peers: Vec::new(),
            error: None,
        };
        assert!(
            render_peer_overview(&overview, None, false)
                .contains("Recent activity is unavailable in this state.")
        );
    }

    #[test]
    fn peers_command_history_policy_skips_disabled_and_configuration_error() {
        assert!(!should_load_history(PeerOverviewState::Disabled));
        assert!(!should_load_history(PeerOverviewState::ConfigurationError));
        assert!(should_load_history(PeerOverviewState::Enabled));
        assert!(should_load_history(PeerOverviewState::Unlisted));
    }

    #[test]
    fn peers_command_renderer_shows_safe_live_state_ambient_and_latest_activity() {
        let overview = PeerOverview {
            state: PeerOverviewState::Enabled,
            identity: Some(PeerIdentityInfo {
                alias: "Jcode".to_string(),
                group: "reviewers".to_string(),
                project: "jcode".to_string(),
            }),
            peers: vec![
                PeerInfo {
                    alias: "Ready".to_string(),
                    group: "reviewers".to_string(),
                    project: "ready-project".to_string(),
                    state: PeerState::Idle,
                },
                PeerInfo {
                    alias: "Busy".to_string(),
                    group: "reviewers".to_string(),
                    project: "busy-project".to_string(),
                    state: PeerState::Busy,
                },
                PeerInfo {
                    alias: "Offline".to_string(),
                    group: "reviewers".to_string(),
                    project: "offline-project".to_string(),
                    state: PeerState::Offline,
                },
                PeerInfo {
                    alias: "Ambiguous".to_string(),
                    group: "reviewers".to_string(),
                    project: "ambiguous-project".to_string(),
                    state: PeerState::Ambiguous,
                },
            ],
            error: None,
        };
        let activity = PeerActivityReport {
            activities: vec![
                PeerActivity {
                    occurred_at: Some(Utc.with_ymd_and_hms(2026, 8, 6, 18, 0, 0).unwrap()),
                    direction: PeerActivityDirection::Outbound,
                    peer_alias: "Ready".to_string(),
                    peer_project: Some("ready-project".to_string()),
                    outcome: PeerActivityOutcome::Replied,
                },
                PeerActivity {
                    occurred_at: None,
                    direction: PeerActivityDirection::Inbound,
                    peer_alias: "Busy".to_string(),
                    peer_project: Some("busy-project".to_string()),
                    outcome: PeerActivityOutcome::Received,
                },
            ],
            history_limited: true,
            read_errors: 1,
        };

        let rendered = render_peer_overview(&overview, Some(&activity), false);

        for expected in [
            "Peer Messaging: ON",
            "You: Jcode (`jcode`) · group `reviewers`",
            "Ambient initiation: OFF (peer initiation unavailable)",
            "● Ready · ready · ready-project",
            "◐ Busy · busy · busy-project",
            "○ Offline · offline · offline-project",
            "! Ambiguous · ambiguous · ambiguous-project",
            "→ Ready (`ready-project`) · replied",
            "time unavailable ← Busy (`busy-project`) · received",
            "Some older activity was skipped to keep this view fast.",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?} in {rendered:?}"
            );
        }
        for forbidden in [
            "PRIVATE BODY",
            "working_dir",
            "session_id",
            "capability",
            "Message ID",
            "peer_123",
            "C:\\Users\\micha",
        ] {
            assert!(!rendered.contains(forbidden));
        }
    }

    #[test]
    fn peers_command_renderer_handles_disabled_config_error_and_unlisted_without_secrets() {
        let disabled = PeerOverview {
            state: PeerOverviewState::Disabled,
            identity: None,
            peers: Vec::new(),
            error: None,
        };
        let disabled_rendered = render_peer_overview(&disabled, None, true);
        assert!(disabled_rendered.contains("Peer Messaging: OFF"));
        assert!(disabled_rendered.contains("Ambient initiation: ON (peer initiation unavailable)"));

        let invalid = PeerOverview {
            state: PeerOverviewState::ConfigurationError,
            identity: None,
            peers: Vec::new(),
            error: Some("Could not read C:\\private\\peer-groups.json".to_string()),
        };
        let invalid_rendered = render_peer_overview(&invalid, None, false);
        assert!(invalid_rendered.contains("Peer Messaging: CONFIGURATION ERROR"));
        assert!(!invalid_rendered.contains("C:\\private"));

        let unlisted = PeerOverview {
            state: PeerOverviewState::Unlisted,
            identity: None,
            peers: Vec::new(),
            error: None,
        };
        let unlisted_rendered = render_peer_overview(
            &unlisted,
            Some(&PeerActivityReport {
                activities: Vec::new(),
                history_limited: false,
                read_errors: 0,
            }),
            false,
        );
        assert!(unlisted_rendered.contains("This project is not listed"));
    }
}
