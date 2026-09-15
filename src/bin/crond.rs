//! `crond` — the cron daemon.

use std::fs::OpenOptions;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use chrono::{Duration, NaiveDateTime, Offset, TimeZone};
use clap::Parser;
use nix::fcntl::{Flock, FlockArg};

use crontab_rs::config::Config;
use crontab_rs::daemon::{Pass, Scheduler};
use crontab_rs::database::Database;
use crontab_rs::job::Runner;
use crontab_rs::logging;
use crontab_rs::mail::Mailer;

#[derive(Parser, Debug)]
#[command(
    name = "crond",
    version = concat!(env!("CARGO_PKG_VERSION"), " (cronie-compatible)"),
    about = "Daemon to execute scheduled commands"
)]
struct Cli {
    /// Stay in the foreground and log to stderr as well as syslog.
    #[arg(short = 'n')]
    foreground: bool,
    /// Alias for -n.
    #[arg(short = 'f', hide = true)]
    foreground_f: bool,
    /// Permit any crontab (skip ownership and mode checks).
    #[arg(short = 'p')]
    permit_any: bool,
    /// Send job output to syslog instead of mail.
    #[arg(short = 's')]
    syslog_output: bool,
    /// Mail command reading an RFC 822 message on stdin, or `off`.
    #[arg(short = 'm', value_name = "COMMAND")]
    mail: Option<String>,
    /// Enable debug logging (flag names are accepted for compatibility).
    #[arg(short = 'x', value_name = "FLAGS")]
    debug: Option<String>,
    /// Inherit PATH from the daemon's environment.
    #[arg(short = 'P')]
    inherit_path: bool,
    /// Cluster mode: run user crontabs only on the host named in
    /// <spool>/.cron.hostname (set with `crontab -n`).
    #[arg(short = 'c')]
    cluster: bool,
    /// Accepted for cronie compatibility (crontabs are polled each minute).
    #[arg(short = 'i')]
    no_inotify: bool,
    /// Evaluate one local minute ("YYYY-MM-DD HH:MM"), run due jobs in the
    /// foreground, wait for them and exit.
    #[arg(long, value_name = "TIME")]
    run_at: Option<String>,
}

fn main() -> ExitCode {
    let cli = match crontab_rs::cli::parse_args::<Cli>() {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    let foreground = cli.foreground || cli.foreground_f || cli.run_at.is_some();
    let cfg = Config::from_env();
    let mail_charset = mail_charset();
    let _ = cli.no_inotify;

    if !foreground && let Err(e) = nix::unistd::daemon(false, false) {
        let _ = writeln!(std::io::stderr(), "crond: can't daemonize: {e}");
        return ExitCode::FAILURE;
    }
    logging::init("crond", foreground, cli.debug.is_some());

    // cronie: -s logs output to syslog and -m off discards it; otherwise mail
    // through /usr/sbin/sendmail, or use syslog when it isn't installed.
    let (mailer, syslog_output) = if cli.syslog_output {
        (Mailer::Off, true)
    } else if let Some(m) = &cli.mail {
        (Mailer::from_arg(m), false)
    } else {
        match Mailer::detect() {
            Mailer::Off => {
                log::info!("(CRON) INFO (Syslog will be used instead of sendmail.)");
                (Mailer::Off, true)
            }
            found => (found, false),
        }
    };
    let hostname = nix::unistd::gethostname()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".into());
    let runner = Arc::new(Runner {
        default_path: cfg.default_path.clone(),
        default_shell: cfg.default_shell.clone(),
        inherit_path: cli.inherit_path,
        mailer,
        syslog_output,
        mail_charset,
        hostname,
    });
    let random_scale: f64 = rand::random();
    let db = Database::new(cfg.clone(), cli.permit_any, random_scale);
    let mut sched = Scheduler::new(db, runner, cli.cluster, cli.run_at.is_none());

    if let Some(spec) = &cli.run_at {
        let t = match NaiveDateTime::parse_from_str(spec, "%Y-%m-%d %H:%M") {
            Ok(t) => t,
            Err(e) => {
                log::error!("bad --run-at time \"{spec}\": {e}");
                return ExitCode::FAILURE;
            }
        };
        // A wall time inside a DST gap has no local instant; use the offset
        // in effect just before the gap (an hour earlier), then just after.
        let offset_at = |nt: NaiveDateTime| {
            chrono::Local
                .from_local_datetime(&nt)
                .earliest()
                .map(|d| d.offset().fix().local_minus_utc())
        };
        let gmtoff = offset_at(t)
            .or_else(|| offset_at(t - Duration::hours(1)))
            .or_else(|| offset_at(t + Duration::hours(1)))
            .unwrap_or(0);
        let minute = t.and_utc().timestamp().div_euclid(60);
        sched.db.refresh();
        let mut failed = 0usize;
        for h in sched.run_minute(minute, gmtoff, gmtoff, Pass::All) {
            match h.join() {
                Ok(true) => {}
                Ok(false) => failed += 1,
                Err(_) => {
                    log::error!("(CRON) ERROR (job thread panicked)");
                    failed += 1;
                }
            }
        }
        failed += sched.take_dispatch_failures();
        return if failed == 0 {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    // Single-instance lock + pid file.
    let pid_file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&cfg.pid_file)
    {
        Ok(f) => f,
        Err(e) => {
            log::error!(
                "(CRON) DEATH (can't open pid file {}: {e})",
                cfg.pid_file.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let mut lock = match Flock::lock(pid_file, FlockArg::LockExclusiveNonblock) {
        Ok(l) => l,
        Err((_, e)) => {
            log::error!(
                "(CRON) DEATH (can't lock {}, otherwise running? {e})",
                cfg.pid_file.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let pid_written = lock
        .set_len(0)
        .and_then(|()| writeln!(lock, "{}", std::process::id()))
        .and_then(|()| lock.flush());
    if let Err(e) = pid_written {
        log::error!(
            "(CRON) ERROR (can't write pid file {}: {e})",
            cfg.pid_file.display()
        );
    }

    let term = Arc::new(AtomicBool::new(false));
    let hup = Arc::new(AtomicBool::new(false));
    let registered = [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT]
        .into_iter()
        .map(|sig| (sig, &term))
        .chain(std::iter::once((signal_hook::consts::SIGHUP, &hup)))
        .try_for_each(|(sig, flag)| signal_hook::flag::register(sig, Arc::clone(flag)).map(|_| ()));
    if let Err(e) = registered {
        log::error!("(CRON) DEATH (can't install signal handler: {e})");
        return ExitCode::FAILURE;
    }

    log::info!("(CRON) STARTUP ({})", env!("CARGO_PKG_VERSION"));
    sched.db.refresh();

    // @reboot jobs run once per boot: the marker lives on a tmpfs (/run).
    if !cfg.reboot_file.exists() {
        match std::fs::File::create(&cfg.reboot_file) {
            Ok(_) => {
                sched.run_reboot();
                sched.take_dispatch_failures();
            }
            Err(e) => log::error!(
                "(CRON) INFO (can't create {}: {e}; skipping @reboot jobs)",
                cfg.reboot_file.display()
            ),
        }
    }

    sched.run_forever(&term, &hup);
    log::info!("(CRON) INFO (shutting down)");
    drop(lock);
    let _ = std::fs::remove_file(&cfg.pid_file);
    ExitCode::SUCCESS
}

/// cronie's default mail charset: the codeset of the locale named by the
/// environment (`setlocale(LC_ALL, "")` then `nl_langinfo(CODESET)`).
fn mail_charset() -> String {
    // SAFETY: called at startup before any other thread exists; the returned
    // pointer is read immediately.
    unsafe {
        libc::setlocale(libc::LC_ALL, c"".as_ptr());
        let codeset = libc::nl_langinfo(libc::CODESET);
        if codeset.is_null() {
            "US-ASCII".to_string()
        } else {
            std::ffi::CStr::from_ptr(codeset)
                .to_string_lossy()
                .into_owned()
        }
    }
}
