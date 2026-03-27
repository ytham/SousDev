use tracing::subscriber::set_global_default;
use tracing_subscriber::EnvFilter;

/// Initialise the global tracing subscriber.
///
/// `level` is used as the default filter when the `RUST_LOG` environment
/// variable is not set (e.g. `"info"`, `"debug"`).
/// When `pretty` is `true` a human-readable multi-line format is emitted;
/// otherwise compact single-line JSON-style output is used.
pub fn init_logger(level: &str, pretty: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    if pretty {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .pretty()
            .finish();
        set_global_default(subscriber).ok();
    } else {
        let subscriber = tracing_subscriber::fmt().with_env_filter(filter).finish();
        set_global_default(subscriber).ok();
    }
}

/// A lightweight structured logger that tags every message with a name prefix.
///
/// Wrap a workflow or stage name once at construction time and call the
/// convenience methods throughout the component's lifetime.
///
/// All output goes through [`tracing`] so it is captured by the global
/// subscriber and its log level is honoured.
#[derive(Clone, Debug)]
pub struct Logger {
    /// The prefix string emitted before every log message.
    pub prefix: String,
}

impl Logger {
    /// Create a new [`Logger`] with the given prefix.
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    /// Log at `INFO` level.
    pub fn info(&self, msg: &str) {
        tracing::info!("[{}] {}", self.prefix, msg);
    }

    /// Log at `ERROR` level.
    pub fn error(&self, msg: &str) {
        tracing::error!("[{}] {}", self.prefix, msg);
    }

    /// Log at `DEBUG` level.
    pub fn debug(&self, msg: &str) {
        tracing::debug!("[{}] {}", self.prefix, msg);
    }

    /// Log at `WARN` level.
    pub fn warn(&self, msg: &str) {
        tracing::warn!("[{}] {}", self.prefix, msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logger_new_stores_prefix() {
        let log = Logger::new("my-pipeline");
        assert_eq!(log.prefix, "my-pipeline");
    }

    #[test]
    fn logger_clone() {
        let log = Logger::new("stage");
        let log2 = log.clone();
        assert_eq!(log2.prefix, "stage");
    }

    /// Smoke test: calling the log methods must not panic.
    /// (We cannot easily assert on tracing output without a custom subscriber,
    /// but at least we verify the calls compile and execute without error.)
    #[test]
    fn logger_methods_do_not_panic() {
        let log = Logger::new("test");
        log.info("hello");
        log.warn("watch out");
        log.error("something broke");
        log.debug("verbose detail");
    }
}
