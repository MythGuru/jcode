use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RunStatus {
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunResult {
    pub(crate) schema_version: u32,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) finished_at: DateTime<Utc>,
    pub(crate) duration_ms: u128,
    pub(crate) binary: PathBuf,
    pub(crate) working_directory: PathBuf,
    pub(crate) child_arguments: Vec<String>,
    pub(crate) startup_expected: Vec<String>,
    pub(crate) command: String,
    pub(crate) expected: Vec<String>,
    pub(crate) forbidden: Vec<String>,
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) timeout_secs: u64,
    pub(crate) stable_ms: u64,
    pub(crate) settle_ms: u64,
    pub(crate) status: RunStatus,
    pub(crate) reason: String,
    pub(crate) process_id: Option<u32>,
    pub(crate) exit_status: Option<String>,
    pub(crate) raw_ansi_path: Option<PathBuf>,
    pub(crate) screen_text_path: Option<PathBuf>,
    pub(crate) result_json_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ArtifactPaths {
    pub(crate) raw_ansi: PathBuf,
    pub(crate) screen_text: PathBuf,
    pub(crate) result_json: PathBuf,
}

pub(crate) fn write_artifacts(
    dir: &Path,
    raw: &[u8],
    screen: &str,
    result: &mut RunResult,
) -> Result<ArtifactPaths> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create artifact directory {}", dir.display()))?;

    let paths = ArtifactPaths {
        raw_ansi: dir.join("raw.ansi"),
        screen_text: dir.join("screen.txt"),
        result_json: dir.join("result.json"),
    };

    fs::write(&paths.raw_ansi, raw)
        .with_context(|| format!("failed to write {}", paths.raw_ansi.display()))?;
    let screen = format!("{}\n", screen.trim_end_matches('\n'));
    fs::write(&paths.screen_text, screen)
        .with_context(|| format!("failed to write {}", paths.screen_text.display()))?;

    result.raw_ansi_path = Some(paths.raw_ansi.clone());
    result.screen_text_path = Some(paths.screen_text.clone());
    result.result_json_path = Some(paths.result_json.clone());
    let json = serde_json::to_vec_pretty(result).context("failed to serialize result.json")?;
    fs::write(&paths.result_json, json)
        .with_context(|| format!("failed to write {}", paths.result_json.display()))?;

    Ok(paths)
}

#[cfg(test)]
impl RunResult {
    fn test_fixture(status: RunStatus, reason: &str) -> Self {
        let now = Utc::now();
        Self {
            schema_version: 1,
            started_at: now,
            finished_at: now,
            duration_ms: 0,
            binary: PathBuf::from("jcode.exe"),
            working_directory: PathBuf::from("workspace"),
            child_arguments: vec!["--no-update".to_string()],
            startup_expected: vec!["jcode:d:".to_string()],
            command: "/peers".to_string(),
            expected: vec!["Peer Messaging".to_string()],
            forbidden: vec!["CONFIGURATION ERROR".to_string()],
            rows: 40,
            cols: 120,
            timeout_secs: 30,
            stable_ms: 750,
            settle_ms: 750,
            status,
            reason: reason.to_string(),
            process_id: None,
            exit_status: None,
            raw_ansi_path: None,
            screen_text_path: None,
            result_json_path: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_raw_screen_and_pretty_result_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let mut result = RunResult::test_fixture(RunStatus::Passed, "all assertions matched");
        let paths = write_artifacts(dir.path(), b"\x1b[2JPlanner", "Planner", &mut result).unwrap();

        assert_eq!(std::fs::read(&paths.raw_ansi).unwrap(), b"\x1b[2JPlanner");
        assert_eq!(
            std::fs::read_to_string(&paths.screen_text).unwrap(),
            "Planner\n"
        );
        let json = std::fs::read_to_string(&paths.result_json).unwrap();
        assert!(json.contains("\"schema_version\": 1"));
        assert!(json.contains("\"status\": \"passed\""));
    }

    #[test]
    fn reports_a_non_directory_artifact_destination() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("blocked");
        std::fs::write(&destination, "not a directory").unwrap();
        let mut result = RunResult::test_fixture(RunStatus::Failed, "test failure");

        let error = write_artifacts(&destination, b"", "", &mut result).unwrap_err();

        assert!(
            error
                .to_string()
                .contains(destination.to_string_lossy().as_ref())
        );
    }
}
