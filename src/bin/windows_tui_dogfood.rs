use anyhow::{Context, Result, bail};
use artifacts::{RunResult, RunStatus, write_artifacts};
use chrono::Utc;
use clap::Parser;
use portable_pty::{CommandBuilder, ExitStatus, PtySize, native_pty_system};
use screen::{ScreenObserver, evaluate_assertions};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

#[path = "windows_tui_dogfood/artifacts.rs"]
mod artifacts;
#[path = "windows_tui_dogfood/screen.rs"]
mod screen;

#[derive(Debug, Clone, Parser)]
#[command(about = "Drive a real Jcode TUI in a pseudo terminal and save evidence")]
struct Args {
    #[arg(long)]
    binary: PathBuf,
    #[arg(long)]
    cwd: PathBuf,
    #[arg(long = "arg", allow_hyphen_values = true, value_name = "VALUE")]
    child_args: Vec<OsString>,
    #[arg(long, default_value = "/peers")]
    command: String,
    #[arg(long = "expect", required = true)]
    expected: Vec<String>,
    #[arg(long = "forbid")]
    forbidden: Vec<String>,
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
    #[arg(long, default_value_t = 750)]
    stable_ms: u64,
    #[arg(long, default_value_t = 750)]
    settle_ms: u64,
    #[arg(long, default_value_t = 40)]
    rows: u16,
    #[arg(long, default_value_t = 120)]
    cols: u16,
    #[arg(long)]
    artifacts: Option<PathBuf>,
}

impl Args {
    fn validate(&self) -> Result<()> {
        if !self.binary.is_file() {
            bail!(
                "binary does not exist or is not a file: {}",
                self.binary.display()
            );
        }
        if !self.cwd.is_dir() {
            bail!(
                "working directory does not exist or is not a directory: {}",
                self.cwd.display()
            );
        }
        if self.command.trim().is_empty() {
            bail!("--command must not be empty");
        }
        if self.expected.is_empty() || self.expected.iter().any(|value| value.is_empty()) {
            bail!("at least one non-empty --expect value is required");
        }
        if self.forbidden.iter().any(|value| value.is_empty()) {
            bail!("--forbid values must not be empty");
        }
        if self.timeout_secs == 0 {
            bail!("--timeout-secs must be greater than zero");
        }
        if self.stable_ms == 0 {
            bail!("--stable-ms must be greater than zero");
        }
        if self.settle_ms == 0 {
            bail!("--settle-ms must be greater than zero");
        }
        if self.rows == 0 {
            bail!("--rows must be greater than zero");
        }
        if self.cols == 0 {
            bail!("--cols must be greater than zero");
        }
        Ok(())
    }

    fn artifact_directory(&self, started_at: chrono::DateTime<Utc>) -> PathBuf {
        self.artifacts.clone().unwrap_or_else(|| {
            PathBuf::from("target")
                .join("tui-dogfood")
                .join(started_at.format("%Y%m%dT%H%M%S%.3fZ").to_string())
        })
    }
}

fn main() -> Result<()> {
    let status = run(Args::parse())?;
    if status == RunStatus::Failed {
        std::process::exit(1);
    }
    Ok(())
}

#[derive(Debug)]
struct ExecutionOutcome {
    status: RunStatus,
    reason: String,
    process_id: Option<u32>,
    exit_status: Option<String>,
}

impl ExecutionOutcome {
    fn passed(process_id: Option<u32>) -> Self {
        Self {
            status: RunStatus::Passed,
            reason: "all assertions matched on a stable post-command screen".to_string(),
            process_id,
            exit_status: None,
        }
    }

    fn failed(reason: impl Into<String>, process_id: Option<u32>) -> Self {
        Self {
            status: RunStatus::Failed,
            reason: reason.into(),
            process_id,
            exit_status: None,
        }
    }
}

fn run(args: Args) -> Result<RunStatus> {
    let started_at = Utc::now();
    let started = Instant::now();
    let artifact_dir = args.artifact_directory(started_at);
    let mut raw = Vec::new();
    let mut final_screen = String::new();

    let mut outcome = match args.validate() {
        Ok(()) => {
            let mut observer = ScreenObserver::new(args.rows, args.cols, Instant::now());
            let outcome = match execute_pty(&args, &mut observer, &mut raw) {
                Ok(outcome) => outcome,
                Err(error) => ExecutionOutcome::failed(format!("PTY run failed: {error:#}"), None),
            };
            final_screen = observer.text().to_string();
            outcome
        }
        Err(error) => ExecutionOutcome::failed(format!("validation failed: {error}"), None),
    };

    let finished_at = Utc::now();
    let mut result = RunResult {
        schema_version: 1,
        started_at,
        finished_at,
        duration_ms: started.elapsed().as_millis(),
        binary: args.binary.clone(),
        working_directory: args.cwd.clone(),
        child_arguments: args
            .child_args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect(),
        command: args.command.clone(),
        expected: args.expected.clone(),
        forbidden: args.forbidden.clone(),
        rows: args.rows,
        cols: args.cols,
        timeout_secs: args.timeout_secs,
        stable_ms: args.stable_ms,
        settle_ms: args.settle_ms,
        status: outcome.status,
        reason: std::mem::take(&mut outcome.reason),
        process_id: outcome.process_id,
        exit_status: outcome.exit_status,
        raw_ansi_path: None,
        screen_text_path: None,
        result_json_path: None,
    };
    let paths = write_artifacts(&artifact_dir, &raw, &final_screen, &mut result)?;

    println!("status: {:?}", result.status);
    println!("reason: {}", result.reason);
    println!("artifacts: {}", paths.result_json.display());
    Ok(result.status)
}

fn execute_pty(
    args: &Args,
    observer: &mut ScreenObserver,
    raw: &mut Vec<u8>,
) -> Result<ExecutionOutcome> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: args.rows,
            cols: args.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open native pseudo terminal")?;
    let portable_pty::PtyPair { master, slave } = pair;

    let mut command = CommandBuilder::new(&args.binary);
    command.args(&args.child_args);
    command.cwd(&args.cwd);
    command.env("TERM", "xterm-256color");
    let mut child = slave
        .spawn_command(command)
        .with_context(|| format!("failed to spawn {}", args.binary.display()))?;
    drop(slave);
    let process_id = child.process_id();

    let setup = (|| -> Result<_> {
        let reader = master
            .try_clone_reader()
            .context("failed to clone pseudo-terminal reader")?;
        let writer = master
            .take_writer()
            .context("failed to take pseudo-terminal writer")?;
        Ok((spawn_reader(reader), writer))
    })();

    let (receiver, mut writer) = match setup {
        Ok(setup) => setup,
        Err(error) => {
            let (exit_status, cleanup_error) = cleanup_child(&mut child, None);
            let mut outcome =
                ExecutionOutcome::failed(format!("PTY setup failed: {error:#}"), process_id);
            outcome.exit_status = exit_status.map(|status| status.to_string());
            append_cleanup_error(&mut outcome.reason, cleanup_error);
            drop(master);
            return Ok(outcome);
        }
    };

    let loop_result = drive_terminal(args, &mut child, &mut writer, &receiver, observer, raw);
    let observed_exit = loop_result
        .as_ref()
        .ok()
        .and_then(|result| result.observed_exit.clone());
    let (exit_status, cleanup_error) = cleanup_child(&mut child, observed_exit);
    drop(writer);
    drop(master);
    drain_reader(&receiver, observer, raw, Duration::from_millis(250));

    let mut outcome = match loop_result {
        Ok(result) if result.passed => ExecutionOutcome::passed(process_id),
        Ok(result) => ExecutionOutcome::failed(result.reason, process_id),
        Err(error) => {
            ExecutionOutcome::failed(format!("terminal drive failed: {error:#}"), process_id)
        }
    };
    outcome.exit_status = exit_status.map(|status| status.to_string());
    append_cleanup_error(&mut outcome.reason, cleanup_error);
    Ok(outcome)
}

type ReaderMessage = std::result::Result<Vec<u8>, String>;

fn spawn_reader(mut reader: Box<dyn Read + Send>) -> Receiver<ReaderMessage> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let mut buffer = vec![0_u8; 8192];
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    buffer.truncate(read);
                    if sender.send(Ok(buffer)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(format!("pseudo-terminal read failed: {error}")));
                    break;
                }
            }
        }
    });
    receiver
}

#[derive(Debug)]
struct DriveResult {
    passed: bool,
    reason: String,
    observed_exit: Option<ExitStatus>,
}

fn drive_terminal(
    args: &Args,
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    writer: &mut Box<dyn Write + Send>,
    receiver: &Receiver<ReaderMessage>,
    observer: &mut ScreenObserver,
    raw: &mut Vec<u8>,
) -> Result<DriveResult> {
    let deadline = Instant::now() + Duration::from_secs(args.timeout_secs);
    let startup_stability = Duration::from_millis(args.stable_ms);
    let result_stability = Duration::from_millis(args.settle_ms);
    let mut command_sent = false;
    let mut screen_at_send = String::new();
    let mut saw_post_command_change = false;

    loop {
        receive_one(receiver, observer, raw, Duration::from_millis(25))?;
        drain_available(receiver, observer, raw)?;
        let now = Instant::now();

        if let Some(status) = child.try_wait().context("failed to poll PTY child")? {
            return Ok(DriveResult {
                passed: false,
                reason: format!("child exited before verification completed: {status}"),
                observed_exit: Some(status),
            });
        }

        if !command_sent && observer.is_stable(now, startup_stability) {
            screen_at_send = observer.text().to_string();
            writer
                .write_all(args.command.as_bytes())
                .context("failed to write command to pseudo terminal")?;
            writer
                .write_all(b"\r")
                .context("failed to write Enter to pseudo terminal")?;
            writer
                .flush()
                .context("failed to flush pseudo-terminal input")?;
            command_sent = true;
        }

        if command_sent {
            saw_post_command_change |= observer.text() != screen_at_send;
            if saw_post_command_change {
                let assertions =
                    evaluate_assertions(observer.text(), &args.expected, &args.forbidden);
                if let Some(forbidden) = assertions.forbidden_match {
                    return Ok(DriveResult {
                        passed: false,
                        reason: format!("forbidden text appeared: {forbidden}"),
                        observed_exit: None,
                    });
                }
                if assertions.passed() && observer.is_stable(now, result_stability) {
                    return Ok(DriveResult {
                        passed: true,
                        reason: String::new(),
                        observed_exit: None,
                    });
                }
            }
        }

        if now >= deadline {
            let reason = if !command_sent {
                format!(
                    "startup timed out before a stable non-empty screen; final screen:\n{}",
                    observer.text()
                )
            } else if !saw_post_command_change {
                "verification timed out because the screen never changed after the command"
                    .to_string()
            } else {
                let assertions =
                    evaluate_assertions(observer.text(), &args.expected, &args.forbidden);
                format!(
                    "verification timed out; missing required text: {}",
                    assertions.missing.join(", ")
                )
            };
            return Ok(DriveResult {
                passed: false,
                reason,
                observed_exit: None,
            });
        }
    }
}

fn receive_one(
    receiver: &Receiver<ReaderMessage>,
    observer: &mut ScreenObserver,
    raw: &mut Vec<u8>,
    wait: Duration,
) -> Result<()> {
    match receiver.recv_timeout(wait) {
        Ok(message) => process_reader_message(message, observer, raw),
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Ok(()),
    }
}

fn drain_available(
    receiver: &Receiver<ReaderMessage>,
    observer: &mut ScreenObserver,
    raw: &mut Vec<u8>,
) -> Result<()> {
    for message in receiver.try_iter() {
        process_reader_message(message, observer, raw)?;
    }
    Ok(())
}

fn process_reader_message(
    message: ReaderMessage,
    observer: &mut ScreenObserver,
    raw: &mut Vec<u8>,
) -> Result<()> {
    let bytes = message.map_err(anyhow::Error::msg)?;
    raw.extend_from_slice(&bytes);
    observer.process(&bytes, Instant::now());
    Ok(())
}

fn drain_reader(
    receiver: &Receiver<ReaderMessage>,
    observer: &mut ScreenObserver,
    raw: &mut Vec<u8>,
    limit: Duration,
) {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        let wait = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(wait) {
            Ok(Ok(bytes)) => {
                raw.extend_from_slice(&bytes);
                observer.process(&bytes, Instant::now());
            }
            Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => break,
        }
    }
}

fn cleanup_child(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    observed_exit: Option<ExitStatus>,
) -> (Option<ExitStatus>, Option<String>) {
    if observed_exit.is_some() {
        return (observed_exit, None);
    }

    match child.try_wait() {
        Ok(Some(status)) => (Some(status), None),
        Ok(None) => match child.kill() {
            Ok(()) => match child.wait() {
                Ok(status) => (Some(status), None),
                Err(error) => (
                    None,
                    Some(format!("failed to wait after killing child: {error}")),
                ),
            },
            Err(error) => (
                None,
                Some(format!("failed to kill owned PTY child: {error}")),
            ),
        },
        Err(error) => (
            None,
            Some(format!(
                "failed to poll owned PTY child during cleanup: {error}"
            )),
        ),
    }
}

fn append_cleanup_error(reason: &mut String, cleanup_error: Option<String>) {
    if let Some(error) = cleanup_error {
        if !reason.is_empty() {
            reason.push_str("; ");
        }
        reason.push_str(&error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn valid_args() -> Args {
        Args {
            binary: std::env::current_exe().unwrap(),
            cwd: std::env::current_dir().unwrap(),
            child_args: vec![],
            command: "/peers".to_string(),
            expected: vec!["Peer Messaging".to_string()],
            forbidden: vec![],
            timeout_secs: 30,
            stable_ms: 750,
            settle_ms: 750,
            rows: 40,
            cols: 120,
            artifacts: None,
        }
    }

    #[test]
    fn validation_rejects_zero_dimensions_and_missing_expectations() {
        let mut args = valid_args();
        args.rows = 0;
        assert!(args.validate().unwrap_err().to_string().contains("rows"));

        let mut args = valid_args();
        args.expected.clear();
        assert!(
            args.validate()
                .unwrap_err()
                .to_string()
                .contains("--expect")
        );
    }

    #[test]
    fn validation_rejects_missing_binary_before_spawning() {
        let mut args = valid_args();
        args.binary = PathBuf::from("missing-jcode.exe");
        assert!(args.validate().unwrap_err().to_string().contains("binary"));
    }
}
