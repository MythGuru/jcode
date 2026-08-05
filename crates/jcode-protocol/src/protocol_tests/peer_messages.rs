#[test]
fn peer_send_roundtrip_preserves_hidden_caller_envelope() -> Result<()> {
    let request = Request::PeerSend {
        id: 41,
        caller: PeerCaller {
            session_id: "sender-session".to_string(),
            generation: 7,
            capability: "opaque-secret".to_string(),
        },
        to: "Atlas".to_string(),
        message: "Please review this design.".to_string(),
        tldr: Some("Review design".to_string()),
    };

    let json = serde_json::to_string(&request)?;
    assert!(json.contains("\"type\":\"peer_send\""));
    assert!(json.contains("\"capability\":\"opaque-secret\""));
    let decoded = parse_request_json(&json)?;
    assert_eq!(decoded.id(), 41);
    assert!(decoded.is_lightweight_control_request());
    match decoded {
        Request::PeerSend {
            caller,
            to,
            message,
            tldr,
            ..
        } => {
            assert_eq!(caller.session_id, "sender-session");
            assert_eq!(caller.generation, 7);
            assert_eq!(caller.capability, "opaque-secret");
            assert_eq!(to, "Atlas");
            assert_eq!(message, "Please review this design.");
            assert_eq!(tldr.as_deref(), Some("Review design"));
        }
        other => return Err(anyhow!("unexpected request: {other:?}")),
    }
    Ok(())
}

#[test]
fn peer_list_reply_and_cancel_requests_are_lightweight_and_generation_bound() -> Result<()> {
    let caller = PeerCaller {
        session_id: "session".to_string(),
        generation: 9,
        capability: "secret".to_string(),
    };
    let requests = [
        Request::PeerList {
            id: 1,
            caller: caller.clone(),
        },
        Request::PeerReply {
            id: 2,
            caller: caller.clone(),
            message: "Reviewed.".to_string(),
        },
        Request::PeerCancel { id: 3, caller },
    ];

    for (expected_id, request) in (1_u64..).zip(requests) {
        let json = serde_json::to_string(&request)?;
        let decoded = parse_request_json(&json)?;
        assert_eq!(decoded.id(), expected_id);
        assert!(decoded.is_lightweight_control_request());
    }
    Ok(())
}

#[test]
fn peer_list_result_roundtrip_preserves_only_allowlisted_peer_metadata() -> Result<()> {
    let event = ServerEvent::PeerListResult {
        id: 5,
        peers: vec![PeerInfo {
            alias: "Atlas".to_string(),
            group: "product".to_string(),
            project: "tracker".to_string(),
            state: PeerState::Busy,
        }],
    };

    let json = serde_json::to_string(&event)?;
    assert!(!json.contains("session_id"));
    assert!(!json.contains("working_dir"));
    let decoded = parse_event_json(&json)?;
    match decoded {
        ServerEvent::PeerListResult { id, peers } => {
            assert_eq!(id, 5);
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].alias, "Atlas");
            assert_eq!(peers[0].group, "product");
            assert_eq!(peers[0].project, "tracker");
            assert_eq!(peers[0].state, PeerState::Busy);
        }
        other => return Err(anyhow!("unexpected event: {other:?}")),
    }
    Ok(())
}

#[test]
fn peer_send_result_roundtrip_preserves_reply_and_failure_outcome() -> Result<()> {
    let event = ServerEvent::PeerSendResult {
        id: 8,
        result: PeerResult {
            status: PeerOutcome::Failed,
            message_id: "peer_123".to_string(),
            from: "Atlas".to_string(),
            to: "Jcode".to_string(),
            reply: Some("The shape is sound.".to_string()),
            error: Some("Recipient turn failed after replying.".to_string()),
        },
    };

    let json = serde_json::to_string(&event)?;
    let decoded = parse_event_json(&json)?;
    match decoded {
        ServerEvent::PeerSendResult { id, result } => {
            assert_eq!(id, 8);
            assert_eq!(result.status, PeerOutcome::Failed);
            assert_eq!(result.message_id, "peer_123");
            assert_eq!(result.from, "Atlas");
            assert_eq!(result.to, "Jcode");
            assert_eq!(result.reply.as_deref(), Some("The shape is sound."));
            assert_eq!(
                result.error.as_deref(),
                Some("Recipient turn failed after replying.")
            );
        }
        other => return Err(anyhow!("unexpected event: {other:?}")),
    }
    Ok(())
}

#[test]
fn peer_reply_and_cancel_acknowledgements_roundtrip() -> Result<()> {
    let events = [
        ServerEvent::PeerReplyAccepted {
            id: 11,
            message_id: "peer_reply".to_string(),
        },
        ServerEvent::PeerCancelled {
            id: 12,
            message_id: "peer_cancel".to_string(),
        },
    ];

    for event in events {
        let json = serde_json::to_string(&event)?;
        let decoded = parse_event_json(&json)?;
        match decoded {
            ServerEvent::PeerReplyAccepted { id, message_id } => {
                assert_eq!(id, 11);
                assert_eq!(message_id, "peer_reply");
            }
            ServerEvent::PeerCancelled { id, message_id } => {
                assert_eq!(id, 12);
                assert_eq!(message_id, "peer_cancel");
            }
            other => return Err(anyhow!("unexpected event: {other:?}")),
        }
    }
    Ok(())
}
