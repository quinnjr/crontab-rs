//! Logging to syslog (facility `cron`) with optional stderr mirroring.

use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use log::{Level, LevelFilter, Log, Metadata, Record};
use syslog::{Facility, Formatter3164, LoggerBackend};

type Syslog = syslog::Logger<LoggerBackend, Formatter3164>;

/// How long to wait before trying to reach syslog again after a failed
/// connection, so an absent socket doesn't cost a connect on every message.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(10);

struct SyslogState {
    logger: Option<Syslog>,
    /// No connection attempts before this instant.
    retry_after: Option<Instant>,
}

/// Whether a reconnect may be attempted now.
fn may_reconnect(retry_after: Option<Instant>, now: Instant) -> bool {
    retry_after.is_none_or(|t| now >= t)
}

struct CronLogger {
    syslog: Mutex<SyslogState>,
    /// Copy every message to stderr.
    mirror: bool,
    /// Write to stderr when syslog is unreachable, even if `mirror` is off.
    fallback_stderr: bool,
    ident: String,
}

impl CronLogger {
    /// Send to syslog, connecting (or reconnecting once) as needed. Returns
    /// whether the message was delivered.
    fn send_syslog(&self, level: Level, msg: &str) -> bool {
        let Ok(mut state) = self.syslog.lock() else {
            return false;
        };
        for _ in 0..2 {
            if state.logger.is_none() {
                let now = Instant::now();
                if !may_reconnect(state.retry_after, now) {
                    return false;
                }
                state.logger = connect(&self.ident);
                if state.logger.is_none() {
                    state.retry_after = Some(now + RECONNECT_BACKOFF);
                    return false;
                }
                state.retry_after = None;
            }
            let Some(logger) = state.logger.as_mut() else {
                return false;
            };
            let sent = match level {
                Level::Error => logger.err(msg),
                Level::Warn => logger.warning(msg),
                Level::Info => logger.info(msg),
                Level::Debug | Level::Trace => logger.debug(msg),
            };
            if sent.is_ok() {
                return true;
            }
            // The connection broke; reconnect once right away.
            state.logger = None;
        }
        false
    }
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
        let delivered = self.send_syslog(record.level(), &msg);
        if self.mirror || (self.fallback_stderr && !delivered) {
            let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{now} {}[{}]: {msg}", self.ident, std::process::id());
        }
    }

    fn flush(&self) {}
}

fn connect(ident: &str) -> Option<Syslog> {
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
    match nix::sys::stat::fstat(std::io::stderr()) {
        Ok(st) => st.st_dev == dev && st.st_ino == ino,
        Err(_) => false,
    }
}

/// Install the global logger. `stderr` mirrors messages to standard error
/// (foreground mode), except when stderr is the systemd journal and syslog
/// is reachable.
pub fn init(ident: &str, stderr: bool, debug: bool) {
    let logger = CronLogger {
        syslog: Mutex::new(SyslogState {
            logger: connect(ident),
            retry_after: None,
        }),
        mirror: stderr && !stderr_is_journal(),
        fallback_stderr: stderr,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnects_are_rate_limited() {
        let now = Instant::now();
        assert!(may_reconnect(None, now));
        assert!(!may_reconnect(Some(now + RECONNECT_BACKOFF), now));
        assert!(may_reconnect(Some(now), now));
    }
}
