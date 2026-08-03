use std::sync::OnceLock;

use tracing_subscriber::{EnvFilter, Registry, reload};
use vidcull_ipc::protocol::LogLevel;

static RELOAD_HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

pub fn install_reload_handle(handle: reload::Handle<EnvFilter, Registry>) {
    let _ = RELOAD_HANDLE.set(handle);
}

fn level_directive(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    }
}

pub fn set_log_level(level: LogLevel) -> Result<(), String> {
    let handle = RELOAD_HANDLE
        .get()
        .ok_or_else(|| "log-level reload handle is not installed".to_owned())?;
    handle
        .reload(EnvFilter::new(level_directive(level)))
        .map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_level_maps_to_a_valid_filter_directive() {
        for level in [
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
        ] {
            let directive = level_directive(level);
            assert!(
                EnvFilter::try_new(directive).is_ok(),
                "directive {directive:?} for {level:?} must be a valid EnvFilter"
            );
        }
    }

    #[test]
    fn set_log_level_without_handle_errors_gracefully() {
        assert!(set_log_level(LogLevel::Debug).is_err());
    }
}
