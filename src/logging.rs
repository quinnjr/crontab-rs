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

/// True when stderr is connected to the systemd journal, which systemd
/// advertises as `JOURNAL_STREAM=<device>:<inode>`. Mirroring log lines to
/// stderr there would record every message twice.
fn stderr_is_journal() -> bool {
    let Ok(stream) = std::env::var("JOURNAL_STREAM") else {
        return false;
    };
    let Some((dev, ino)) = stream.split_once(':') else {
        return false;
    };
    let (Ok(dev), Ok(ino)) = (dev.parse::<u64>(), ino.parse::<u64>()) else {
        return false;
    };
    // SAFETY: fstat on fd 2 with a zeroed, correctly sized stat buffer.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(2, &mut st) } != 0 {
        return false;
    }
    st.st_dev as u64 == dev && st.st_ino as u64 == ino
}

/// Install the global logger.  `stderr` mirrors messages to standard error
/// (used in foreground mode and by the `crontab` command for errors).
pub fn init(ident: &str, stderr: bool, debug: bool) {
    let logger = CronLogger {
        syslog: Mutex::new(connect(ident)),
        stderr: stderr && !stderr_is_journal(),
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
