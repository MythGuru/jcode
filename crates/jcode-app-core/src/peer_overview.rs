use crate::protocol::{PeerOverview, Request, ServerEvent};
use anyhow::{Result, anyhow};

const PEER_OVERVIEW_REQUEST_ID: u64 = 1;

fn decode_peer_overview_response(event: ServerEvent) -> Result<PeerOverview> {
    match event {
        ServerEvent::PeerOverviewResult { overview, .. } => Ok(overview),
        ServerEvent::Error { message, .. } => Err(anyhow!(message)),
        event => Err(anyhow!("Unexpected peer overview response: {event:?}")),
    }
}

pub async fn fetch_peer_overview(session_id: &str) -> Result<PeerOverview> {
    let request = Request::PeerOverview {
        id: PEER_OVERVIEW_REQUEST_ID,
        session_id: session_id.to_string(),
    };
    let event = crate::tool::send_lightweight_request(request).await?;
    decode_peer_overview_response(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PeerIdentityInfo, PeerOverviewState};

    #[test]
    fn peer_overview_client_accepts_only_the_expected_result_or_server_error() {
        let expected = PeerOverview {
            state: PeerOverviewState::Enabled,
            identity: Some(PeerIdentityInfo {
                alias: "Jcode".to_string(),
                group: "reviewers".to_string(),
                project: "jcode".to_string(),
            }),
            peers: Vec::new(),
            error: None,
        };
        let decoded = decode_peer_overview_response(ServerEvent::PeerOverviewResult {
            id: PEER_OVERVIEW_REQUEST_ID,
            overview: expected.clone(),
        })
        .expect("overview result");
        assert_eq!(decoded, expected);

        let error = decode_peer_overview_response(ServerEvent::Error {
            id: PEER_OVERVIEW_REQUEST_ID,
            message: "session detached".to_string(),
            retry_after_secs: None,
        })
        .expect_err("server error");
        assert_eq!(error.to_string(), "session detached");

        let unexpected = decode_peer_overview_response(ServerEvent::Pong {
            id: PEER_OVERVIEW_REQUEST_ID,
        })
        .expect_err("unexpected response");
        assert!(
            unexpected
                .to_string()
                .contains("Unexpected peer overview response")
        );
    }
}
