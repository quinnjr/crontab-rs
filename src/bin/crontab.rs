//! `crontab` — install, list, edit and remove per-user crontabs.

use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, ExitCode};

use clap::Parser;
use nix::sys::stat::{Mode, umask};
use nix::unistd::{User, chown, getuid};

use crontab_rs::allow::user_allowed;
use crontab_rs::config::{Config, is_privileged_binary};
use crontab_rs::crontab::{Crontab, Format, ParseError};
use crontab_rs::privs::{Creds, as_real_user, configure_child, gid, is_root};

#[derive(Parser, Debug)]
#[command(
    name = "crontab",
    version = concat!(env!("CARGO_PKG_VERSION"), " (cronie-compatible)"),
    about = "Maintain crontab files for individual users",
    override_usage = "crontab [-u user] <file | ->\n       crontab [-u user] <-l | -r | -e> [-i]\n       crontab -T <file | ->\n       crontab -n <host> | -c"
)]
struct Cli {
    /// Operate on this user's crontab (root only).
    #[arg(short = 'u', value_name = "USER")]
    user: Option<String>,
    /// List the current crontab.
    #[arg(short = 'l', group = "action")]
    list: bool,
    /// Remove the current crontab.
    #[arg(short = 'r', group = "action")]
    remove: bool,
    /// Edit the current crontab with $VISUAL or $EDITOR.
    #[arg(short = 'e', group = "action")]
    edit: bool,
    /// Test a crontab file for syntax errors without installing it.
    #[arg(short = 'T', group = "action")]
    test: bool,
    /// Set the cluster host that runs user crontabs (root only).
    #[arg(short = 'n', group = "action", value_name = "HOST")]
    set_host: Option<String>,
    /// Show the cluster host.
    #[arg(short = 'c', group = "action")]
    show_host: bool,
    /// Prompt before removing with -r (accepted and ignored with other
    /// actions, like cronie).
    #[arg(short = 'i')]
    interactive: bool,
    /// Crontab file to install or test; `-` reads standard input.
    file: Option<String>,
}

fn fail(msg: impl std::fmt::Display) -> ExitCode {
    eprintln!("crontab: {msg}");
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    // Everything this program creates (spool dir, spool files, edit temp
    // files) is private; never inherit a permissive umask from the caller.
    umask(Mode::from_bits_truncate(0o077));

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // cronie exits 1 on usage errors; help and version exit 0.
            let _ = e.print();
            return match e.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    ExitCode::SUCCESS
                }
                _ => ExitCode::FAILURE,
            };
        }
    };
    let cfg = Config::from_env();

    let file_action =
        !(cli.list || cli.remove || cli.edit || cli.set_host.is_some() || cli.show_host);
    if !file_action && cli.file.is_some() {
        return fail("a file argument cannot be combined with -l, -r, -e, -n or -c");
    }

    let real_uid = getuid();
    let me = match User::from_uid(real_uid) {
        Ok(Some(u)) => u,
        Ok(None) => return fail("your UID isn't in the passwd file, bailing out"),
        Err(e) => return fail(format!("can't look up UID {}: {e}", real_uid.as_raw())),
    };

    if cli.test {
        return test_file(cli.file.as_deref().unwrap_or("-"));
    }
    if cli.show_host {
        return match fs::read_to_string(cfg.spool_dir.join(".cron.hostname")) {
            Ok(h) => {
                println!("{}", h.trim());
                ExitCode::SUCCESS
            }
            Err(_) => fail("no cluster host is set"),
        };
    }

    if !user_allowed(&cfg, &me.name, real_uid.as_raw()) {
        eprintln!(
            "You ({}) are not allowed to use this program (crontab)",
            me.name
        );
        eprintln!("See crontab(1) for more information");
        return ExitCode::FAILURE;
    }

    if let Some(host) = &cli.set_host {
        if !real_uid.is_root() {
            return fail("must be privileged to set the cluster host");
        }
        let path = cfg.spool_dir.join(".cron.hostname");
        let r = if host.is_empty() {
            fs::remove_file(&path).or_else(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(e)
                }
            })
        } else {
            fs::write(&path, format!("{host}\n"))
        };
        return match r {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => fail(format!("{}: {e}", path.display())),
        };
    }

    let target = match &cli.user {
        None => me.clone(),
        Some(name) => {
            if !real_uid.is_root() && *name != me.name {
                return fail("must be privileged to use -u");
            }
            match User::from_name(name) {
                Ok(Some(u)) => u,
                _ => return fail(format!("user `{name}' unknown")),
            }
        }
    };
    let spool_file = cfg.user_crontab(&target.name);

    if cli.list {
        return match fs::read(&spool_file) {
            Ok(bytes) => {
                let mut out = io::stdout().lock();
                if out.write_all(&bytes).and_then(|_| out.flush()).is_err() {
                    return ExitCode::FAILURE;
                }
                ExitCode::SUCCESS
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fail(format!("no crontab for {}", target.name))
            }
            Err(e) => fail(format!("{}: {e}", spool_file.display())),
        };
    }

    if cli.remove {
        match fs::symlink_metadata(&spool_file) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return fail(format!("no crontab for {}", target.name));
            }
            Err(e) => return fail(format!("{}: {e}", spool_file.display())),
        }
        if cli.interactive
            && !confirm(&format!(
                "crontab: really delete {}'s crontab? (y/n) ",
                target.name
            ))
        {
            return ExitCode::SUCCESS;
        }
        return match fs::remove_file(&spool_file) {
            Ok(()) => {
                touch_spool(&cfg);
                ExitCode::SUCCESS
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fail(format!("no crontab for {}", target.name))
            }
            Err(e) => fail(format!("unable to delete {}: {e}", spool_file.display())),
        };
    }

    if cli.edit {
        return edit(&cfg, &me, &target, &spool_file);
    }

    // Install from file or stdin.
    let source = cli.file.as_deref().unwrap_or("-");
    let text = match read_input(source) {
        Ok(t) => t,
        Err(e) => return fail(format!("{source}: {e}")),
    };
    match Crontab::parse_as(&text, Format::User, target.uid.is_root()) {
        Ok(tab) => print_warnings(&tab),
        Err(errors) => {
            report_errors(source, &errors);
            eprintln!("errors in crontab file, can't install.");
            return ExitCode::FAILURE;
        }
    }
    match install(&cfg, &target, &text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(format!("installing new crontab failed: {e}")),
    }
}

/// Read a file (as the invoking user, so a set-uid binary cannot be used to
/// read files the caller couldn't) or stdin for `-`.
fn read_input(source: &str) -> io::Result<String> {
    let bytes = if source == "-" {
        let mut buf = Vec::new();
        io::stdin().lock().read_to_end(&mut buf)?;
        buf
    } else {
        as_real_user(|| fs::read(source))?
    };
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not valid UTF-8"))
}

/// cronie prints parse warnings, such as oversized steps, to stderr.
fn print_warnings(tab: &Crontab) {
    for w in &tab.warnings {
        eprintln!("{w}");
    }
}

fn report_errors(source: &str, errors: &[ParseError]) {
    for e in errors {
        eprintln!("\"{source}\":{}: {}", e.line, e.error);
    }
}

fn test_file(source: &str) -> ExitCode {
    let text = match read_input(source) {
        Ok(t) => t,
        Err(e) => return fail(format!("{source}: {e}")),
    };
    match Crontab::parse_as(&text, Format::User, getuid().is_root()) {
        Ok(tab) => {
            print_warnings(&tab);
            println!("No syntax issues were found in the crontab file.");
            ExitCode::SUCCESS
        }
        Err(errors) => {
            report_errors(source, &errors);
            eprintln!("Invalid crontab file. Syntax issues were found.");
            ExitCode::FAILURE
        }
    }
}

fn touch_spool(cfg: &Config) {
    // Directory mtime changes on rename/unlink already; this is belt and braces.
    if let Ok(f) = fs::File::open(&cfg.spool_dir) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

/// Atomically replace the user's spool file.
fn install(cfg: &Config, target: &User, text: &str) -> io::Result<()> {
    if !cfg.spool_dir.is_dir() {
        if is_root() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&cfg.spool_dir)?;
            fs::set_permissions(&cfg.spool_dir, fs::Permissions::from_mode(0o700))?;
        } else {
            return Err(io::Error::other(format!(
                "spool directory {} does not exist",
                cfg.spool_dir.display()
            )));
        }
    }
    let mut body = text.to_string();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".tmp.{}.", target.name))
        .tempfile_in(&cfg.spool_dir)?;
    tmp.write_all(body.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    if is_root() {
        chown(tmp.path(), Some(target.uid), Some(gid(target.gid.as_raw())))
            .map_err(io::Error::other)?;
    }
    tmp.persist(cfg.user_crontab(&target.name))
        .map_err(|e| e.error)?;
    touch_spool(cfg);
    Ok(())
}

fn confirm(prompt: &str) -> bool {
    loop {
        eprint!("{prompt}");
        let _ = io::stderr().flush();
        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => return false,
            Ok(_) => match line.trim().chars().next() {
                Some('y') | Some('Y') => return true,
                Some('n') | Some('N') => return false,
                _ => eprintln!("Please enter Y or N"),
            },
        }
    }
}

fn editor() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string())
}

/// Result of an edit session: the exit code and whether the temp file must
/// be left behind for the user (their edits could not be installed).
struct EditOutcome {
    code: ExitCode,
    keep_file: bool,
}

impl EditOutcome {
    fn done(code: ExitCode) -> Self {
        EditOutcome {
            code,
            keep_file: false,
        }
    }
    fn keep(code: ExitCode) -> Self {
        EditOutcome {
            code,
            keep_file: true,
        }
    }
}

fn edit(cfg: &Config, me: &User, target: &User, spool_file: &Path) -> ExitCode {
    let original = match fs::read_to_string(spool_file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("no crontab for {} - using an empty one", target.name);
            String::new()
        }
        Err(e) => return fail(format!("{}: {e}", spool_file.display())),
    };

    // The editor runs as the invoking user; resolve its credentials without
    // any NSS lookup so a lookup failure can never leave it running as root.
    let creds = match Creds::real_user() {
        Ok(c) => c,
        Err(e) => return fail(format!("can't determine credentials for {}: {e}", me.name)),
    };

    // The temp file is created, edited and removed as the invoking user. A
    // set-ID binary ignores the caller-controlled $TMPDIR.
    let tmp = match as_real_user(|| -> io::Result<tempfile::TempPath> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("crontab.");
        let mut t = if is_privileged_binary() {
            builder.tempfile_in("/tmp")?
        } else {
            builder.tempfile()?
        };
        t.write_all(original.as_bytes())?;
        t.flush()?;
        Ok(t.into_temp_path())
    }) {
        Ok(t) => t,
        Err(e) => return fail(format!("can't create temp file: {e}")),
    };

    let outcome = edit_session(cfg, target, &original, &tmp, &creds);

    // Single cleanup point, always performed with the invoker's privileges.
    if outcome.keep_file {
        match tmp.keep() {
            Ok(path) => eprintln!("crontab: edits left in {}", path.display()),
            Err(e) => eprintln!("crontab: edits left in {}", e.path.display()),
        }
    } else if let Err(e) = as_real_user(|| tmp.close()) {
        eprintln!("crontab: can't remove temp file: {e}");
    }
    outcome.code
}

fn edit_session(
    cfg: &Config,
    target: &User,
    original: &str,
    tmp_path: &Path,
    creds: &Creds,
) -> EditOutcome {
    let editor = editor();

    loop {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(tmp_path);
        if let Err(e) = configure_child(&mut cmd, Some(creds.clone()), false, None) {
            return EditOutcome::done(fail(format!("can't prepare editor: {e}")));
        }
        match cmd.status() {
            Ok(s) if s.success() => {}
            Ok(s) => {
                return EditOutcome::done(fail(format!(
                    "\"{editor}\" exited with {s}; no changes made to crontab"
                )));
            }
            Err(e) => {
                return EditOutcome::done(fail(format!("could not run \"{editor}\": {e}")));
            }
        }

        let edited = match as_real_user(|| fs::read_to_string(tmp_path)) {
            Ok(t) => t,
            Err(e) => {
                return EditOutcome::keep(fail(format!("can't read edited file: {e}")));
            }
        };
        if edited == original {
            eprintln!("crontab: no changes made to crontab");
            return EditOutcome::done(ExitCode::SUCCESS);
        }
        match Crontab::parse_as(&edited, Format::User, target.uid.is_root()) {
            Ok(tab) => {
                print_warnings(&tab);
                return match install(cfg, target, &edited) {
                    Ok(()) => {
                        eprintln!("crontab: installing new crontab");
                        EditOutcome::done(ExitCode::SUCCESS)
                    }
                    Err(e) => {
                        EditOutcome::keep(fail(format!("installing new crontab failed: {e}")))
                    }
                };
            }
            Err(errors) => {
                report_errors(&tmp_path.display().to_string(), &errors);
                eprintln!("errors in crontab file, can't install.");
                if !confirm("Do you want to retry the same edit? (y/n) ") {
                    return EditOutcome::keep(ExitCode::FAILURE);
                }
            }
        }
    }
}
