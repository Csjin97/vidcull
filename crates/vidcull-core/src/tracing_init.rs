use std::sync::OnceLock;

use tracing_subscriber::{EnvFilter, fmt};

static INIT: OnceLock<()> = OnceLock::new();

pub const DEFAULT_FILTER: &str = "info";

pub fn init_tracing() {
    INIT.get_or_init(|| {
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
        let _ = fmt().with_env_filter(filter).with_target(true).try_init();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_tracing_is_idempotent() {
        init_tracing();
        init_tracing();
        init_tracing();
        assert!(INIT.get().is_some());
    }

    #[test]
    fn default_filter_constant_is_info() {
        assert_eq!(DEFAULT_FILTER, "info");
    }
}
