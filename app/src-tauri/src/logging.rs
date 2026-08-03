use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;

fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(
        || std::env::temp_dir().join("vidcull"),
        |local| PathBuf::from(local).join("vidcull"),
    )
}

#[must_use]
pub fn init_file_logging() -> Option<WorkerGuard> {
    use tracing_appender::rolling;
    use tracing_subscriber::prelude::*;

    let log_dir = data_dir().join("logs");
    if std::fs::create_dir_all(&log_dir).is_err() {
        eprintln!("[vidcull] could not create log directory; UI file logging disabled");
        return None;
    }

    let appender = rolling::Builder::new()
        .rotation(rolling::Rotation::DAILY)
        .max_log_files(7)
        .filename_prefix("vidcull-app")
        .filename_suffix("log")
        .build(&log_dir)
        .ok()?;
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(non_blocking),
        )
        .try_init();

    Some(guard)
}

fn render_panic(info: &std::panic::PanicHookInfo<'_>) -> (String, String) {
    let payload = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned());
    let location = info.location().map_or_else(
        || "<unknown location>".to_owned(),
        |l| format!("{}:{}:{}", l.file(), l.line(), l.column()),
    );
    (payload, location)
}

pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let (payload, location) = render_panic(info);
        let backtrace = std::backtrace::Backtrace::capture();
        tracing::error!(
            panic.payload = %payload,
            panic.location = %location,
            panic.backtrace = %backtrace,
            "UI process panicked",
        );
        previous(info);
    }));
}

#[cfg(test)]
mod panic_hook_tests {
    use std::panic;
    use std::sync::Mutex;

    static CAPTURED: Mutex<Option<(String, String)>> = Mutex::new(None);

    #[test]
    fn render_panic_extracts_payload_and_location() {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(|info| {
            *CAPTURED.lock().unwrap() = Some(super::render_panic(info));
        }));

        let formatted = panic::catch_unwind(|| panic!("boom {}", 42));
        let (payload, location) = CAPTURED.lock().unwrap().take().expect("hook captured");
        let static_str = panic::catch_unwind(|| panic!("plain boom"));
        let (payload2, _) = CAPTURED.lock().unwrap().take().expect("hook captured");

        panic::set_hook(previous);
        assert!(formatted.is_err() && static_str.is_err());
        assert_eq!(payload, "boom 42");
        assert_eq!(payload2, "plain boom");
        assert!(location.contains("logging.rs"), "location was {location}");
    }
}
