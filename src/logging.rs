//! Logging to syslog (facility `cron`) with optional stderr mirroring.

use std::io::Write;
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};
use syslog::{Facility, Formatter3164, LoggerBackend};

struct CronLogger {
    syslog: Mutex<Option<syslog::Logger<LoggerBackend, Formatter3164>>>,
    stderr: bool,
    ident: String,
}

impl Log for CronLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let msg = record.args().to_string();
        if self.stderr {
            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{now} {}[{}]: {msg}", self.ident, std::process::id());
        }
        if let Ok(mut guard) = self.syslog.lock()
            && let Some(logger) = guard.as_mut()
        {
            let r = match record.level() {
                Level::Error => logger.err(&msg),
                Level::Warn => logger.warning(&msg),
                Level::Info => logger.info(&msg),
                Level::Debug | Level::Trace => logger.debug(&msg),
            };
            if r.is_err() {
                // Syslog went away; try to reconnect next time.
                *guard = connect(&self.ident);
            }
        }
    }

    fn flush(&self) {}
}

fn connect(ident: &str) -> Option<syslog::Logger<LoggerBackend, Formatter3164>> {
    let formatter = Formatter3164 {
        facility: Facility::LOG_CRON,
        hostname: None,
        process: ident.to_string(),
        pid: std::process::id(),
    };
    syslog::unix(formatter).ok()
}

/// Install the global logger.  `stderr` mirrors messages to standard error
/// (used in foreground mode and by the `crontab` command for errors).
pub fn init(ident: &str, stderr: bool, debug: bool) {
    let logger = CronLogger {
        syslog: Mutex::new(connect(ident)),
        stderr,
        ident: ident.to_string(),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(if debug {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        });
    }
}
