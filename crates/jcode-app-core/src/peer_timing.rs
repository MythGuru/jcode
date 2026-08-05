use std::time::Duration;

pub(crate) const PEER_RECIPIENT_DEADLINE: Duration = Duration::from_secs(10 * 60);
const PEER_SOCKET_GRACE: Duration = Duration::from_secs(30);

pub(crate) fn peer_socket_timeout() -> Duration {
    PEER_RECIPIENT_DEADLINE.saturating_add(PEER_SOCKET_GRACE)
}
