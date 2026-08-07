use std::time::{Duration, Instant};

pub(crate) struct ScreenObserver {
    parser: vt100::Parser,
    text: String,
    last_changed: Instant,
}

impl ScreenObserver {
    pub(crate) fn new(rows: u16, cols: u16, now: Instant) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            text: String::new(),
            last_changed: now,
        }
    }

    pub(crate) fn process(&mut self, bytes: &[u8], now: Instant) {
        self.parser.process(bytes);
        let text = normalize_screen(&self.parser.screen().contents());
        if text != self.text {
            self.text = text;
            self.last_changed = now;
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn is_stable(&self, now: Instant, interval: Duration) -> bool {
        !self.text.is_empty() && now.saturating_duration_since(self.last_changed) >= interval
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AssertionState {
    pub(crate) missing: Vec<String>,
    pub(crate) forbidden_match: Option<String>,
}

impl AssertionState {
    pub(crate) fn passed(&self) -> bool {
        self.missing.is_empty() && self.forbidden_match.is_none()
    }
}

pub(crate) fn evaluate_assertions(
    text: &str,
    expected: &[String],
    forbidden: &[String],
) -> AssertionState {
    AssertionState {
        missing: expected
            .iter()
            .filter(|value| !text.contains(value.as_str()))
            .cloned()
            .collect(),
        forbidden_match: forbidden
            .iter()
            .find(|value| text.contains(value.as_str()))
            .cloned(),
    }
}

fn normalize_screen(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut rows = normalized
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>();
    while rows.last().is_some_and(|row| row.is_empty()) {
        rows.pop();
    }
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn normalizes_terminal_rows_without_destroying_internal_spacing() {
        let text = normalize_screen("Peer Messaging   \r\nPlanner  core-team  \r\n\r\n");
        assert_eq!(text, "Peer Messaging\nPlanner  core-team");
    }

    #[test]
    fn assertion_state_reports_missing_and_forbidden_text() {
        let expected = vec!["Peer Messaging".to_string(), "Planner".to_string()];
        let forbidden = vec!["CONFIGURATION ERROR".to_string()];
        let state =
            evaluate_assertions("Peer Messaging\nCONFIGURATION ERROR", &expected, &forbidden);

        assert_eq!(state.missing, vec!["Planner"]);
        assert_eq!(
            state.forbidden_match.as_deref(),
            Some("CONFIGURATION ERROR")
        );
        assert!(!state.passed());
    }

    #[test]
    fn screen_is_stable_only_after_non_empty_text_stops_changing() {
        let start = Instant::now();
        let mut observer = ScreenObserver::new(4, 40, start);

        assert!(!observer.is_stable(start + Duration::from_secs(1), Duration::from_millis(500)));
        observer.process(b"Planner", start + Duration::from_millis(10));
        assert_eq!(observer.text(), "Planner");
        assert!(!observer.is_stable(
            start + Duration::from_millis(400),
            Duration::from_millis(500)
        ));
        assert!(observer.is_stable(
            start + Duration::from_millis(510),
            Duration::from_millis(500)
        ));
    }
}
