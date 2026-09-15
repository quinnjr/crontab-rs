//! `crontab` — install, list, edit and remove per-user crontabs.

use std::fs;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, ExitCode};

use clap::Parser;
use nix::sys::stat::{Mode, umask};
use nix::unistd::{User, chown, getuid};

use crontab_rs::allow::user_allowed;
use crontab_rs::config::{Config, is_privileged_binary};
use crontab_rs::crontab::{
    Crontab, Diagnostic, Format, MAX_USER_ENTRIES, MAX_USER_ENVS, ParseOptions, ParseOutput,
};
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

    let cli = match crontab_rs::cli::parse_args::<Cli>() {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    let cfg = Config::from_env();

    let file_action =
        !(cli.list || cli.remove || cli.edit || cli.set_host.is_some() || cli.show_host);
    if !file_action && cli.file.is_some() {
        return fail("a file argument cannot be combined with -l, -r, -e, -n or -c");
    }
    // cronie won't read a new crontab from a terminal without an explicit `-`.
    if file_action
        && crontab_rs::cli::stdin_file_required(cli.file.as_deref(), io::stdin().is_terminal())
    {
        eprintln!("crontab: usage error: file name or - (for stdin) must be specified");
        return ExitCode::FAILURE;
    }

    let real_uid = getuid();
    let me = match User::from_uid(real_uid) {
        Ok(Some(u)) => u,
        Ok(None) => return fail("your UID isn't in the passwd file, bailing out"),
        Err(e) => return fail(format!("can't look up UID {}: {e}", real_uid.as_raw())),
    };

    let target = match &cli.user {
        None => me.clone(),
        Some(name) => {
            // cronie: -u needs root, even for your own name, and can't be
            // combined with -T, -n or -c.
            if !real_uid.is_root() {
                eprintln!("must be privileged to use -u");
                return ExitCode::FAILURE;
            }
            if cli.test {
                eprintln!("cannot use -u with -n, -c or -T");
                return ExitCode::FAILURE;
            }
            if cli.set_host.is_some() || cli.show_host {
                eprintln!("cannot use -u with -n or -c");
                return ExitCode::FAILURE;
            }
            match User::from_name(name) {
                Ok(Some(u)) => u,
                _ => return fail(format!("user `{name}' unknown")),
            }
        }
    };

    if !user_allowed(&cfg, &me.name, real_uid.as_raw()) {
        eprintln!(
            "You ({}) are not allowed to use this program (crontab)",
            me.name
        );
        eprintln!("See crontab(1) for more information");
        return ExitCode::FAILURE;
    }

    if cli.test {
        return test_file(cli.file.as_deref().unwrap_or("-"), &target);
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

    if let Some(host) = &cli.set_host {
        if !real_uid.is_root() {
            eprintln!("must be privileged to set host with -n");
            return ExitCode::FAILURE;
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
    if !check_syntax(source, &text, &target) {
        eprintln!("Invalid crontab file, can't install.");
        return ExitCode::FAILURE;
    }
    match install(&cfg, &target, &text) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(format!("installing new crontab failed: {e}")),
    }
}

/// Read a file (as the invoking user, so a set-uid binary cannot be used to
/// read files the caller couldn't) or stdin for `-`.
fn read_input(source: &str) -> io::Result<Vec<u8>> {
    if source == "-" {
        let mut buf = Vec::new();
        io::stdin().lock().read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        as_real_user(|| fs::read(source))
    }
}

/// Parse `text` as `target`'s crontab. Crontabs need not be UTF-8.
fn parse_for(text: &[u8], target: &User) -> ParseOutput {
    let options = ParseOptions::new(Format::User)
        .privileged(target.uid.is_root())
        .inherit_process_env();
    Crontab::parse_bytes(text, &options)
}

/// cronie's `check_syntax`: print warnings and the first error, stopping
/// there, then check the entry and variable limits on what was read.
/// Returns true when the crontab can be installed.
fn check_syntax(source: &str, text: &[u8], target: &User) -> bool {
    let parsed = parse_for(text, target);
    let mut valid = true;
    for diagnostic in parsed.until_first_error() {
        match diagnostic {
            Diagnostic::Warning(w) => eprintln!("{w}"),
            Diagnostic::Error(e) => {
                eprintln!("\"{source}\":{}: {}", e.line, e.error);
                valid = false;
            }
            Diagnostic::TooMuchGarbage { line } => {
                eprintln!(
                    "\"{source}\":{line}: too much non-parseable content (comments, empty lines, spaces)"
                );
                valid = false;
            }
            Diagnostic::BadRandomDelay { .. } => {}
        }
    }
    let before_stop = |line: usize| parsed.first_error_line().is_none_or(|stop| line < stop);
    let envs = parsed.env_lines.iter().filter(|l| before_stop(**l)).count();
    if envs > MAX_USER_ENVS {
        eprintln!(
            "There are too many environment variables in the crontab file. Limit: {MAX_USER_ENVS}"
        );
        return false;
    }
    let entries = parsed
        .crontab
        .entries
        .iter()
        .filter(|e| before_stop(e.line))
        .count();
    if entries > MAX_USER_ENTRIES {
        eprintln!("There are too many entries in the crontab file. Limit: {MAX_USER_ENTRIES}");
        return false;
    }
    valid
}

fn test_file(source: &str, target: &User) -> ExitCode {
    let text = match read_input(source) {
        Ok(t) => t,
        Err(e) => return fail(format!("{source}: {e}")),
    };
    if check_syntax(source, &text, target) {
        eprintln!("No syntax issues were found in the crontab file.");
        ExitCode::SUCCESS
    } else {
        eprintln!("Invalid crontab file. Syntax issues were found.");
        ExitCode::FAILURE
    }
}

fn touch_spool(cfg: &Config) {
    // Directory mtime changes on rename/unlink already; this is belt and braces.
    if let Ok(f) = fs::File::open(&cfg.spool_dir) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

/// Atomically replace the user's spool file.
fn install(cfg: &Config, target: &User, text: &[u8]) -> io::Result<()> {
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
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".tmp.{}.", target.name))
        .tempfile_in(&cfg.spool_dir)?;
    tmp.write_all(text)?;
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

/// cronie's edit retry prompt: asked on stdout, `Enter Y or N` on stderr for
/// anything else. End of input counts as "no" (cronie would spin forever).
fn ask_retry() -> bool {
    loop {
        print!("Do you want to retry the same edit? (Y/N) ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => return false,
            Ok(_) => match line.chars().next() {
                Some('y') | Some('Y') => return true,
                Some('n') | Some('N') => return false,
                _ => eprintln!("Enter Y or N"),
            },
        }
    }
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
    let original = match fs::read(spool_file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("no crontab for {} - using an empty one", target.name);
            Vec::new()
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
        t.write_all(&original)?;
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
    original: &[u8],
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

        let edited = match as_real_user(|| fs::read(tmp_path)) {
            Ok(t) => t,
            Err(e) => {
                return EditOutcome::keep(fail(format!("can't read edited file: {e}")));
            }
        };
        if edited == original {
            eprintln!("crontab: no changes made to crontab");
            return EditOutcome::done(ExitCode::SUCCESS);
        }
        eprintln!("crontab: installing new crontab");
        if check_syntax(&tmp_path.display().to_string(), &edited, target) {
            return match install(cfg, target, &edited) {
                Ok(()) => EditOutcome::done(ExitCode::SUCCESS),
                // cronie abandons the edit and still exits 0.
                Err(e) => {
                    eprintln!("crontab: installing new crontab failed: {e}");
                    EditOutcome::keep(ExitCode::SUCCESS)
                }
            };
        }
        eprintln!("Invalid crontab file, can't install.");
        if !ask_retry() {
            // cronie exits 0 after leaving the edits behind.
            return EditOutcome::keep(ExitCode::SUCCESS);
        }
    }
}
