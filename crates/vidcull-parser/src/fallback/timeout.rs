use std::io;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use vidcull_core::{Error, Result};

use crate::cancel::Cancel;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) const ENV_TIMEOUT: &str = "AV_FFMPEG_TIMEOUT_SECS";

pub(crate) const PROBE_TIMEOUT_SECS: u64 = 30;

pub(crate) const DECODE_FRAME_TIMEOUT_SECS: u64 = 90;

pub(crate) const BATCH_DECODE_TIMEOUT_SECS: u64 = 300;

pub const RENDER_TIMEOUT_SECS: u64 = 300;

pub const TIMEOUT_TOKEN: &str = "timed out after";

#[must_use]
pub fn effective_timeout(default_secs: u64) -> Duration {
    static OVERRIDE: std::sync::LazyLock<Option<String>> =
        std::sync::LazyLock::new(|| std::env::var(ENV_TIMEOUT).ok());
    timeout_from_env(OVERRIDE.as_deref(), default_secs)
}

fn timeout_from_env(env_val: Option<&str>, default_secs: u64) -> Duration {
    if let Some(val) = env_val {
        if let Ok(n) = val.trim().parse::<u64>() {
            return Duration::from_secs(n);
        }
    }
    Duration::from_secs(default_secs)
}

fn normalize_program_symbol(program: &std::ffi::OsStr) -> String {
    Path::new(program).file_name().map_or_else(
        || "unknown".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

pub fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    caller: &'static str,
) -> Result<Output> {
    run_with_timeout_cancellable(cmd, timeout, Cancel::default(), caller)
}

pub fn run_with_timeout_cancellable(
    cmd: &mut Command,
    timeout: Duration,
    cancel: Cancel<'_>,
    caller: &'static str,
) -> Result<Output> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let program_symbol = normalize_program_symbol(cmd.get_program());
    #[cfg(windows)]
    let no_window = true;
    #[cfg(not(windows))]
    let no_window = false;
    let spawn_result = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match spawn_result {
        Ok(child) => {
            tracing::debug!(
                program = %program_symbol,
                caller,
                pid = child.id(),
                no_window,
                "fallback subprocess spawned"
            );
            child
        }
        Err(err) => {
            tracing::debug!(
                program = %program_symbol,
                caller,
                no_window,
                error = %err,
                "fallback subprocess spawn failed"
            );
            return Err(Error::Io(err));
        }
    };

    let stdout_reader = child.stdout.take().expect("stdout is piped");
    let stderr_reader = child.stderr.take().expect("stderr is piped");

    let (stdout_tx, stdout_rx) = mpsc::channel::<io::Result<Vec<u8>>>();
    let (stderr_tx, stderr_rx) = mpsc::channel::<io::Result<Vec<u8>>>();

    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let result = { io::Read::read_to_end(&mut { stdout_reader }, &mut buf).map(|_| buf) };
        let _ = stdout_tx.send(result);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let result = { io::Read::read_to_end(&mut { stderr_reader }, &mut buf).map(|_| buf) };
        let _ = stderr_tx.send(result);
    });

    let status = poll_until_done(&mut child, timeout, cancel)?;

    let stdout = stdout_rx
        .recv()
        .unwrap_or_else(|_| Ok(Vec::new()))
        .map_err(Error::Io)?;
    let stderr = stderr_rx
        .recv()
        .unwrap_or_else(|_| Ok(Vec::new()))
        .map_err(Error::Io)?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn poll_until_done(
    child: &mut Child,
    timeout: Duration,
    cancel: Cancel<'_>,
) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;

    loop {
        if let Some(status) = child.try_wait().map_err(Error::Io)? {
            return Ok(status);
        }
        if cancel.fired() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Decode(format!(
                "ffmpeg/ffprobe {TIMEOUT_TOKEN} {:.1} s — child killed and reaped",
                timeout.as_secs_f64()
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    fn sleep_cmd(secs: u64) -> Command {
        if cfg!(windows) {
            let mut cmd = Command::new("powershell");
            cmd.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Start-Sleep -Seconds {secs}"),
            ]);
            cmd
        } else {
            let mut cmd = Command::new("sleep");
            cmd.arg(secs.to_string());
            cmd
        }
    }

    #[test]
    fn timeout_kills_wedged_process() {
        let mut cmd = sleep_cmd(60);
        let timeout = Duration::from_millis(300);
        let err = run_with_timeout(&mut cmd, timeout, "test").expect_err("should time out");
        assert!(
            matches!(err, Error::Decode(_)),
            "expected Decode error, got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("timed out"), "message: {msg}");
        assert!(
            msg.contains(TIMEOUT_TOKEN),
            "timeout message must contain TIMEOUT_TOKEN ({TIMEOUT_TOKEN:?}): {msg}",
        );
    }

    #[test]
    fn timeout_error_message_contains_timeout_token() {
        let mut cmd = sleep_cmd(60);
        let timeout = Duration::from_millis(200);
        let err = run_with_timeout(&mut cmd, timeout, "test").expect_err("should time out");
        let Error::Decode(msg) = &err else {
            panic!("expected Error::Decode, got {err:?}");
        };
        assert!(
            msg.contains(TIMEOUT_TOKEN),
            "Error::Decode message must contain TIMEOUT_TOKEN ({TIMEOUT_TOKEN:?}); got: {msg}",
        );
    }

    fn echo_cmd() -> Command {
        if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Write-Output hello",
            ]);
            c
        } else {
            let mut c = Command::new("echo");
            c.arg("hello");
            c
        }
    }

    #[test]
    fn cancel_kills_in_flight_child() {
        let mut cmd = sleep_cmd(60);
        let timeout = Duration::from_secs(30);
        let cancel = AtomicBool::new(true);
        let start = Instant::now();
        let err = run_with_timeout_cancellable(
            &mut cmd,
            timeout,
            Cancel {
                pause: Some(&cancel),
                removal: None,
            },
            "test",
        )
        .expect_err("a set cancel flag must abort the decode");
        assert!(
            matches!(err, Error::Cancelled),
            "expected Error::Cancelled, got {err:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "cancel did not abort promptly: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn cancel_set_mid_decode_aborts_promptly() {
        let mut cmd = sleep_cmd(60);
        let timeout = Duration::from_secs(30);
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flipper = std::sync::Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            flipper.store(true, Ordering::Relaxed);
        });
        let start = Instant::now();
        let err = run_with_timeout_cancellable(
            &mut cmd,
            timeout,
            Cancel {
                pause: None,
                removal: Some(&*cancel),
            },
            "test",
        )
        .expect_err("a mid-flight cancel must abort the decode");
        assert!(
            matches!(err, Error::Cancelled),
            "expected Error::Cancelled, got {err:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "mid-flight cancel did not abort promptly: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn unset_cancel_flag_does_not_abort_normal_process() {
        let cancel = AtomicBool::new(false);
        let mut cmd = echo_cmd();
        let output = run_with_timeout_cancellable(
            &mut cmd,
            Duration::from_secs(10),
            Cancel {
                pause: Some(&cancel),
                removal: None,
            },
            "test",
        )
        .expect("an unset cancel flag must not abort a normal decode");
        assert!(output.status.success(), "exit: {}", output.status);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("hello"), "stdout: {stdout}");
    }

    #[test]
    fn fast_process_completes_within_timeout() {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("powershell");
            c.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Write-Output hello",
            ]);
            c
        } else {
            let mut c = Command::new("echo");
            c.arg("hello");
            c
        };
        let timeout = Duration::from_secs(10);
        let output = run_with_timeout(&mut cmd, timeout, "test").expect("should succeed");
        assert!(output.status.success(), "exit: {}", output.status);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("hello"), "stdout: {stdout}");
    }

    #[test]
    fn timeout_from_env_uses_default_when_absent() {
        assert_eq!(timeout_from_env(None, 42), Duration::from_secs(42));
    }

    #[test]
    fn timeout_from_env_honours_valid_override() {
        assert_eq!(timeout_from_env(Some("99"), 42), Duration::from_secs(99));
    }

    #[test]
    fn timeout_from_env_ignores_invalid_value() {
        assert_eq!(
            timeout_from_env(Some("not-a-number"), 42),
            Duration::from_secs(42)
        );
    }

    #[test]
    fn timeout_from_env_ignores_empty_string() {
        assert_eq!(timeout_from_env(Some(""), 42), Duration::from_secs(42));
    }

    use std::fmt::{self, Write as _};
    use std::sync::{Arc, Mutex as StdMutex};

    use tracing::field::{Field, Visit};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::layer::{Context, Layer};
    use tracing_subscriber::prelude::*;

    #[derive(Default)]
    struct CaptureLayer {
        lines: Arc<StdMutex<Vec<String>>>,
    }

    #[derive(Default)]
    struct FieldVisitor {
        message: String,
        fields: String,
    }

    impl FieldVisitor {
        fn finish(mut self) -> String {
            if !self.fields.is_empty() {
                if self.message.is_empty() {
                    self.message = self.fields.trim_start().to_owned();
                } else {
                    self.message.push_str(&self.fields);
                }
            }
            self.message
        }
    }

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            if field.name() == "message" {
                let _ = write!(self.message, "{value:?}");
            } else {
                let _ = write!(self.fields, " {}={value:?}", field.name());
            }
        }
    }

    impl<S: Subscriber> Layer<S> for CaptureLayer {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.lines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(visitor.finish());
        }
    }

    fn capture_events(f: impl FnOnce()) -> Vec<String> {
        let lines: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let layer = CaptureLayer {
            lines: Arc::clone(&lines),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, f);
        Arc::try_unwrap(lines)
            .unwrap_or_else(|arc| StdMutex::new(arc.lock().unwrap().clone()))
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn nonexistent_program_spawn_failure_emits_event() {
        let events = capture_events(|| {
            let mut cmd = Command::new("this-binary-does-not-exist-204");
            let _ = run_with_timeout(&mut cmd, Duration::from_secs(5), "test");
        });
        assert!(
            events.iter().any(|l| l.contains("spawn failed")),
            "expected a spawn-failure diagnostic event, got: {events:?}"
        );
    }

    #[test]
    fn spawn_event_is_pii_free() {
        let events = capture_events(|| {
            let mut cmd = sleep_cmd(60);
            cmd.arg("C:\\Users\\somebody\\Videos\\my private clip.mp4");
            let _ = run_with_timeout(&mut cmd, Duration::from_millis(200), "test");
        });
        assert!(!events.is_empty(), "expected at least one captured event");
        for line in &events {
            assert!(
                !line.contains("\\Users\\"),
                "event leaked an absolute user-path segment: {line}"
            );
            assert!(
                !line.contains("my private clip"),
                "event leaked the media file name: {line}"
            );
            let has_drive_path = line.as_bytes().windows(3).any(|w| {
                w[0].is_ascii_alphabetic() && w[1] == b':' && (w[2] == b'\\' || w[2] == b'/')
            });
            assert!(!has_drive_path, "event leaked an absolute path: {line}");
        }
    }

    #[test]
    fn spawn_event_carries_caller_tag() {
        let events = capture_events(|| {
            let mut cmd = echo_cmd();
            let _ = run_with_timeout(&mut cmd, Duration::from_secs(10), "probe");
        });
        assert!(
            events
                .iter()
                .any(|l| l.contains("caller=\"probe\"") || l.contains("caller=probe")),
            "expected the caller tag 'probe' attached to the spawn event, got: {events:?}"
        );
    }
}
