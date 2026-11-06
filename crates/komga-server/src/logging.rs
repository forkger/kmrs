use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{InitError, RollingFileAppender};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{reload, EnvFilter, Registry};

/// The log filter when neither `RUST_LOG` nor `[logging] level` sets one.
pub const DEFAULT_FILTER: &str = "warn,kmrs=info,komga=info";

/// Keeps file logging running, and lets the filter change once the config file is read:
/// logging starts first because the config loader logs too (which file it loaded,
/// sections it ignores).
pub struct Logging {
    _guard: Option<WorkerGuard>,
    filter: reload::Handle<EnvFilter, Registry>,
}

impl Logging {
    /// Switches to `[logging] level` from the config file, unless `RUST_LOG` is set.
    pub fn apply_config_level(&self, level: Option<&str>) {
        self.apply(level, std::env::var_os("RUST_LOG").is_some());
    }

    fn apply(&self, level: Option<&str>, rust_log_set: bool) {
        let Some(level) = level.filter(|_| !rust_log_set) else {
            return;
        };
        let result = EnvFilter::try_new(level)
            .map_err(|e| e.to_string())
            .and_then(|filter| self.filter.reload(filter).map_err(|e| e.to_string()));
        if let Err(e) = result {
            tracing::warn!("log level unchanged: {e}");
        }
    }
}

pub fn init(logs_dir: &Path) -> Logging {
    let (filter, handle) = reload::Layer::new(
        EnvFilter::try_from_default_env().unwrap_or_else(|_| DEFAULT_FILTER.into()),
    );
    let stdout = tracing_subscriber::fmt::layer();

    let appender = std::fs::create_dir_all(logs_dir)
        .map_err(|e| e.to_string())
        .and_then(|()| rolling_appender(logs_dir).map_err(|e| e.to_string()));

    match appender {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false);
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout)
                .with(file)
                .init();
            Logging {
                _guard: Some(guard),
                filter: handle,
            }
        }
        Err(e) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout)
                .init();
            tracing::warn!("file logging disabled: {e}");
            Logging {
                _guard: None,
                filter: handle,
            }
        }
    }
}

// The Java version rotates komga.log by size; tracing-appender only rotates by
// time, so daily rotation with 7 kept files approximates its 7-day history.
fn rolling_appender(logs_dir: &Path) -> Result<RollingFileAppender, InitError> {
    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("kmrs")
        .filename_suffix("log")
        .max_log_files(7)
        .build(logs_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountEvents(Arc<AtomicUsize>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountEvents {
        fn on_event(
            &self,
            _event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Debug events under `kmrs`, logged before and after applying `level`.
    fn debug_events(level: &str, rust_log_set: bool) -> (usize, usize) {
        let (filter, handle) = reload::Layer::new(EnvFilter::new(DEFAULT_FILTER));
        let seen = Arc::new(AtomicUsize::new(0));
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(CountEvents(seen.clone()));
        let logging = Logging {
            _guard: None,
            filter: handle,
        };
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "kmrs::test", "before");
            let before = seen.load(Ordering::SeqCst);
            logging.apply(Some(level), rust_log_set);
            tracing::debug!(target: "kmrs::test", "after");
            (before, seen.load(Ordering::SeqCst) - before)
        })
    }

    #[test]
    fn config_level_takes_effect_once_the_config_is_read() {
        assert_eq!(debug_events("warn,kmrs=debug", false), (0, 1));
    }

    #[test]
    fn rust_log_keeps_precedence_over_the_config_level() {
        assert_eq!(debug_events("warn,kmrs=debug", true), (0, 0));
    }

    #[test]
    fn rolling_appender_writes_dated_kmrs_log() {
        let dir = tempfile::tempdir().unwrap();
        let appender = rolling_appender(dir.path()).unwrap();
        let (mut writer, guard) = tracing_appender::non_blocking(appender);
        writeln!(writer, "hello kmrs").unwrap();
        drop(writer);
        drop(guard);

        let files: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 1, "expected a single log file, got {files:?}");
        assert!(
            files[0].starts_with("kmrs.") && files[0].ends_with(".log"),
            "unexpected log file name: {}",
            files[0]
        );
        let content = std::fs::read_to_string(dir.path().join(&files[0])).unwrap();
        assert!(content.contains("hello kmrs"));
    }
}
