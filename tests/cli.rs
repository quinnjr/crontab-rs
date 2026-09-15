//! End-to-end tests driving the `crontab` and `crond` binaries against a
//! temporary spool.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("spool")).unwrap();
        fs::create_dir(dir.path().join("cron.d")).unwrap();
        Env { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn cmd(&self, bin: &str) -> Command {
        let mut c = Command::new(bin);
        c.env("CRONTAB_RS_SPOOL_DIR", self.path("spool"))
            .env("CRONTAB_RS_SYSTEM_CRONTAB", self.path("crontab"))
            .env("CRONTAB_RS_CRON_D", self.path("cron.d"))
            .env("CRONTAB_RS_ALLOW", self.path("cron.allow"))
            .env("CRONTAB_RS_DENY", self.path("cron.deny"))
            .env("CRONTAB_RS_PID_FILE", self.path("crond.pid"))
            .env("CRONTAB_RS_REBOOT_FILE", self.path("crond.reboot"));
        c
    }

    fn crontab(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut c = self.cmd(env!("CARGO_BIN_EXE_crontab"));
        c.args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        if let Some(text) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }

    fn crond(&self) -> Command {
        self.cmd(env!("CARGO_BIN_EXE_crond"))
    }
}

fn me() -> String {
    nix::unistd::User::from_uid(nix::unistd::getuid())
        .unwrap()
        .unwrap()
        .name
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn install_list_remove() {
    let env = Env::new();
    let o = env.crontab(&["-l"], None);
    assert!(!o.status.success());
    assert!(stderr(&o).contains(&format!("no crontab for {}", me())));

    let tab = "MAILTO=\"\"\n*/5 * * * * echo hi\n";
    let o = env.crontab(&["-"], Some(tab));
    assert!(o.status.success(), "{}", stderr(&o));

    let spool = env.path("spool").join(me());
    assert_eq!(
        fs::metadata(&spool).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let o = env.crontab(&["-l"], None);
    assert_eq!(stdout(&o), tab);

    // Install from a file path. As in cronie, the last line needs a newline.
    let file = env.path("mytab");
    fs::write(&file, "@daily true").unwrap();
    let o = env.crontab(&[file.to_str().unwrap()], None);
    assert!(!o.status.success());
    assert!(stderr(&o).contains(":1: premature EOF"), "{}", stderr(&o));
    assert!(stderr(&o).contains("Invalid crontab file, can't install."));
    fs::write(&file, "@daily true\n").unwrap();
    let o = env.crontab(&[file.to_str().unwrap()], None);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&env.crontab(&["-l"], None)), "@daily true\n");

    // As in cronie, -u needs root even for your own name.
    let o = env.crontab(&["-u", &me(), "-l"], None);
    assert_eq!(
        o.status.success(),
        nix::unistd::geteuid().is_root(),
        "{}",
        stderr(&o)
    );

    // -ri with "n" keeps it; -r removes it.
    let o = env.crontab(&["-r", "-i"], Some("n\n"));
    assert!(o.status.success());
    assert!(spool.exists());
    let o = env.crontab(&["-r"], None);
    assert!(o.status.success());
    assert!(!spool.exists());
    let o = env.crontab(&["-r"], None);
    assert!(!o.status.success());
}

#[test]
fn rejects_bad_crontab() {
    let env = Env::new();
    let o = env.crontab(&["-"], Some("* * * * * ok\n61 * * * * bad\n"));
    assert!(!o.status.success());
    let err = stderr(&o);
    assert!(err.contains("\"-\":2: bad minute"), "{err}");
    assert!(err.contains("can't install"));
    assert!(!env.path("spool").join(me()).exists());
}

#[test]
fn test_mode() {
    let env = Env::new();
    let o = env.crontab(&["-T", "-"], Some("0 0 * * * ok\n"));
    assert!(o.status.success());
    // cronie prints the result of -T on stderr.
    assert!(stderr(&o).contains("No syntax issues"), "{}", stderr(&o));
    assert!(stdout(&o).is_empty());
    let o = env.crontab(&["-T", "-"], Some("0 0 * * 9 bad\n"));
    assert!(!o.status.success());
    assert!(stderr(&o).contains("bad day-of-week"));
}

#[test]
fn deny_and_allow() {
    let env = Env::new();
    fs::write(env.path("cron.deny"), format!("{}\n", me())).unwrap();
    let o = env.crontab(&["-l"], None);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("not allowed"));
    fs::write(env.path("cron.allow"), format!("{}\n", me())).unwrap();
    let o = env.crontab(&["-l"], None);
    assert!(stderr(&o).contains("no crontab"), "{}", stderr(&o));
}

#[test]
fn file_arg_conflicts_with_actions() {
    let env = Env::new();
    let o = env.crontab(&["-l", "somefile"], None);
    assert!(!o.status.success());
    let o = env.crontab(&["-l", "-r"], None);
    assert!(!o.status.success());
}

#[test]
fn edit_via_editor() {
    let env = Env::new();
    let script = env.path("editor.sh");
    fs::write(&script, "#!/bin/sh\necho '0 4 * * * edited' >> \"$1\"\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let o = env
        .cmd(env!("CARGO_BIN_EXE_crontab"))
        .arg("-e")
        .env("VISUAL", format!("/bin/sh {}", script.display()))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&env.crontab(&["-l"], None)), "0 4 * * * edited\n");

    // An editor that changes nothing.
    let o = env
        .cmd(env!("CARGO_BIN_EXE_crontab"))
        .arg("-e")
        .env("VISUAL", "true")
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(stderr(&o).contains("no changes made"));

    // An editor producing an invalid file, declining the retry.
    let bad = env.path("bad.sh");
    fs::write(&bad, "#!/bin/sh\necho '99 * * * * nope' >> \"$1\"\n").unwrap();
    fs::set_permissions(&bad, fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = env
        .cmd(env!("CARGO_BIN_EXE_crontab"))
        .arg("-e")
        .env("VISUAL", format!("/bin/sh {}", bad.display()))
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"n\n").unwrap();
    let o = child.wait_with_output().unwrap();
    let err = stderr(&o);
    // cronie leaves the edits behind and exits 0.
    assert!(o.status.success(), "{err}");
    assert!(err.contains("bad minute"), "{err}");
    let left = err
        .lines()
        .find_map(|l| l.strip_prefix("crontab: edits left in "))
        .expect("edits left in message");
    let _ = fs::remove_file(left);
    assert_eq!(stdout(&env.crontab(&["-l"], None)), "0 4 * * * edited\n");
}

#[test]
fn crond_runs_due_jobs() {
    let env = Env::new();
    let out = env.path("out");
    let mbox = env.path("mbox");
    let tab = format!(
        "GREETING=hello\n\
         9 17 * * * echo \"$GREETING $USER\" >> {out}\n\
         10 17 * * * echo wrong-minute >> {out}\n\
         * * 14 9 * cat >> {out}%from stdin\n\
         9 17 * * * echo mailed\n\
         9 17 * * * -n echo not-mailed\n\
         CRON_TZ=Asia/Tokyo\n\
         0 0 * * * echo tz-wrong >> {out}\n",
        out = out.display()
    );
    let o = env.crontab(&["-"], Some(&tab));
    assert!(o.status.success(), "{}", stderr(&o));

    fs::write(
        env.path("cron.d").join("sysjob"),
        format!("9 17 * * * {} echo system >> {}\n", me(), out.display()),
    )
    .unwrap();

    let o = env
        .crond()
        .args(["-p", "-m", &format!("cat >> '{}'", mbox.display())])
        .args(["--run-at", "2026-09-14 17:09"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));

    let mut lines: Vec<String> = fs::read_to_string(&out)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    let mut expected = vec![
        format!("hello {}", me()),
        "from stdin".into(),
        "system".into(),
    ];
    expected.sort();
    assert_eq!(lines, expected);

    let mail = fs::read_to_string(&mbox).unwrap();
    assert!(mail.contains("\n\nmailed\n"));
    assert!(!mail.contains("not-mailed"));
    assert!(stderr(&o).contains("CMD (echo mailed)"), "{}", stderr(&o));
}

#[test]
fn crond_rejects_insecure_system_file_without_p() {
    let env = Env::new();
    let out = env.path("out");
    let sys = env.path("crontab");
    fs::write(
        &sys,
        format!("* * * * * {} echo x >> {}\n", me(), out.display()),
    )
    .unwrap();
    fs::set_permissions(&sys, fs::Permissions::from_mode(0o666)).unwrap();
    let o = env
        .crond()
        .args(["-m", "off", "--run-at", "2026-01-01 00:00"])
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(!out.exists());
    // The test file is created by this (non-root) test process, so it is
    // owned by us rather than root: the ownership check must fire first.
    // As root, the file would be root-owned and the mode check would fire
    // instead.
    if nix::unistd::geteuid().is_root() {
        assert!(stderr(&o).contains("INSECURE MODE"), "{}", stderr(&o));
    } else {
        assert!(stderr(&o).contains("WRONG FILE OWNER"), "{}", stderr(&o));
    }
}

#[test]
fn u_flag_denied_for_other_user() {
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let env = Env::new();
    let o = env.crontab(&["-u", "root", "-l"], None);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("must be privileged to use -u"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn set_cluster_host_requires_root() {
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let env = Env::new();
    let o = env.crontab(&["-n", "somehost"], None);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("must be privileged to set host with -n"),
        "{}",
        stderr(&o)
    );
    assert!(!env.path("spool").join(".cron.hostname").exists());
}

#[test]
fn show_cluster_host_without_hostname_file() {
    let env = Env::new();
    let o = env.crontab(&["-c"], None);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("no cluster host is set"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn show_cluster_host_success() {
    let env = Env::new();
    fs::write(env.path("spool").join(".cron.hostname"), "node7\n").unwrap();
    let o = env.crontab(&["-c"], None);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "node7\n");
}

#[test]
fn set_and_clear_cluster_host_as_root() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let env = Env::new();
    let o = env.crontab(&["-n", "node9"], None);
    assert!(o.status.success(), "{}", stderr(&o));
    let o = env.crontab(&["-c"], None);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "node9\n");

    let o = env.crontab(&["-n", ""], None);
    assert!(o.status.success(), "{}", stderr(&o));
    let o = env.crontab(&["-c"], None);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("no cluster host is set"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn run_at_bad_time_fails() {
    let env = Env::new();
    let o = env
        .crond()
        .args(["-m", "off", "--run-at", "garbage"])
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(stderr(&o).contains("bad --run-at time"), "{}", stderr(&o));
}

#[test]
fn crond_skips_only_jobs_for_unknown_users() {
    let env = Env::new();
    let out = env.path("ran");
    fs::write(
        env.path("cron.d").join("mixed"),
        format!(
            "* * * * * {} touch {}\n* * * * * no-such-user-xyz123 echo x\n",
            me(),
            out.display()
        ),
    )
    .unwrap();
    let o = env
        .crond()
        .args(["-p", "-m", "off", "--run-at", "2026-01-01 00:00"])
        .output()
        .unwrap();
    let err = stderr(&o);
    assert!(out.exists(), "jobs for known users still run: {err}");
    assert!(
        err.contains("(no-such-user-xyz123) ERROR (getpwnam() failed - user unknown)"),
        "{err}"
    );
    assert!(
        !o.status.success(),
        "run-at reports the job that could not run"
    );
    assert_eq!(
        err.matches("getpwnam() failed").count(),
        1,
        "logged once: {err}"
    );
    assert!(!err.contains("ERROR running job for"), "{err}");
}

#[test]
fn crond_keeps_good_lines_when_one_is_bad() {
    let env = Env::new();
    let (a, c) = (env.path("a"), env.path("c"));
    fs::write(
        env.path("cron.d").join("partly-bad"),
        format!(
            "* * * * * {me} touch {a}\n99 * * * * {me} touch nope\n* * * * * {me} touch {c}\n* * * * * {me} touch unterminated",
            me = me(),
            a = a.display(),
            c = c.display()
        ),
    )
    .unwrap();
    let o = env
        .crond()
        .args(["-p", "-m", "off", "--run-at", "2026-01-01 00:00"])
        .output()
        .unwrap();
    let err = stderr(&o);
    assert!(o.status.success(), "{err}");
    assert!(a.exists() && c.exists(), "{err}");
    assert!(err.contains("(CRON) bad minute ("), "{err}");
    assert!(err.contains("(CRON) missing newline before EOF ("), "{err}");
}

#[test]
fn syntax_check_stops_at_first_error() {
    let env = Env::new();
    let o = env.crontab(
        &["-T", "-"],
        Some("*/61 * * * * x\n99 * * * * y\n88 * * * * z\n"),
    );
    assert!(!o.status.success());
    let err = stderr(&o);
    let warning = err.find("Warning: Step size 61").expect("warning printed");
    let first = err
        .find("\"-\":2: bad minute")
        .expect("first error printed");
    assert!(warning < first, "{err}");
    assert!(!err.contains(":3:"), "{err}");
    let o = env.crontab(&["-T", "-"], Some("0 0 * * * x"));
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("\"-\":1: premature EOF"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn u_flag_rules_follow_option_order() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let env = Env::new();
    // -T before -u is refused; -u before -T checks the file as that user.
    let o = env.crontab(&["-T", "-", "-u", "nobody"], Some("0 0 * * * x\n"));
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("cannot use -u with -n, -c or -T"),
        "{}",
        stderr(&o)
    );
    let o = env.crontab(&["-u", "nobody", "-T", "-"], Some("-* * * * * x\n"));
    assert!(!o.status.success());
    assert!(stderr(&o).contains("bad option"), "{}", stderr(&o));
    // -c after -u is refused only for a different user.
    let o = env.crontab(&["-u", "nobody", "-c"], None);
    assert!(
        stderr(&o).contains("cannot use -u with -n or -c"),
        "{}",
        stderr(&o)
    );
    let o = env.crontab(&["-u", "root", "-c"], None);
    assert!(!stderr(&o).contains("cannot use -u"), "{}", stderr(&o));
}

/// Wait for a child, killing it and failing the test after `secs` seconds.
fn wait_or_fail(mut child: std::process::Child, secs: u64, what: &str) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > Duration::from_secs(secs) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what} hung");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn output_write_errors_do_not_panic() {
    let env = Env::new();
    // stdout is a pipe whose reader is already gone.
    let (reader, writer) = nix::unistd::pipe().unwrap();
    drop(reader);
    let o = env
        .cmd(env!("CARGO_BIN_EXE_crontab"))
        .arg("-V")
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(!stderr(&o).contains("panicked"), "{}", stderr(&o));
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let o = env
        .crond()
        .arg("-h")
        .stderr(Stdio::from(full))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn syntax_check_enforces_cronie_limits() {
    let env = Env::new();
    let envs: String = (0..1001).map(|i| format!("V{i}=x\n")).collect();
    let o = env.crontab(&["-T", "-"], Some(&envs));
    assert!(!o.status.success());
    assert!(
        stderr(&o)
            .contains("There are too many environment variables in the crontab file. Limit: 1000"),
        "{}",
        stderr(&o)
    );
    let entries: String = (0..10001).map(|_| "0 0 * * * true\n").collect();
    let o = env.crontab(&["-T", "-"], Some(&entries));
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("There are too many entries in the crontab file. Limit: 10000"),
        "{}",
        stderr(&o)
    );
    let garbage = format!("{}\n0 0 * * * x\n", "#".repeat(40000));
    let o = env.crontab(&["-T", "-"], Some(&garbage));
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains("too much non-parseable content"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn non_utf8_crontabs_keep_their_bytes() {
    use std::os::unix::ffi::OsStrExt;
    let env = Env::new();
    fs::create_dir(env.path("out")).unwrap();
    let target = env
        .path("out")
        .join(std::ffi::OsStr::from_bytes(b"caf\xe9"));
    let mut tab = format!("* * * * * {} touch '", me()).into_bytes();
    tab.extend_from_slice(target.as_os_str().as_bytes());
    tab.extend_from_slice(b"'\n");
    fs::write(env.path("cron.d").join("latin1"), &tab).unwrap();
    let o = env
        .crond()
        .args(["-p", "-m", "off", "--run-at", "2026-01-01 00:00"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        target.exists(),
        "the command's bytes must reach the shell unchanged"
    );

    let user_tab = b"# caf\xe9\n0 0 * * * echo caf\xe9\n".to_vec();
    let mut child = env
        .cmd(env!("CARGO_BIN_EXE_crontab"))
        .arg("-")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&user_tab).unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
    let o = env
        .cmd(env!("CARGO_BIN_EXE_crontab"))
        .arg("-l")
        .output()
        .unwrap();
    assert_eq!(o.stdout, user_tab);
}

#[test]
fn cron_tz_naming_a_fifo_does_not_hang() {
    let env = Env::new();
    let fifo = env.path("tzfifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
    let mut child = env
        .cmd(env!("CARGO_BIN_EXE_crontab"))
        .args(["-T", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("CRON_TZ={}\n* * * * * true\n", fifo.display()).as_bytes())
        .unwrap();
    assert!(wait_or_fail(child, 10, "crontab -T with a CRON_TZ FIFO").success());

    fs::write(
        env.path("cron.d").join("fifo-tz"),
        format!("CRON_TZ={}\n* * * * * {} true\n", fifo.display(), me()),
    )
    .unwrap();
    let child = env
        .crond()
        .args(["-p", "-m", "off", "--run-at", "2026-01-01 00:00"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_or_fail(child, 10, "crond with a CRON_TZ FIFO");
}

#[test]
fn crond_run_at_fails_when_job_cannot_run() {
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let env = Env::new();
    // A spool crontab for root, loaded with -p by a non-root daemon: the
    // job is due but the daemon cannot switch to root to run it.
    fs::write(env.path("spool").join("root"), "* * * * * true\n").unwrap();
    let o = env
        .crond()
        .args(["-p", "-m", "off", "--run-at", "2026-01-01 00:00"])
        .output()
        .unwrap();
    assert!(!o.status.success(), "{}", stderr(&o));
}

#[test]
fn usage_errors_exit_1_like_cronie() {
    let env = Env::new();
    let o = env.crontab(&["--bogus-flag"], None);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let o = env.crond().arg("-Z").output().unwrap();
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    // cronie prints help to stderr and exits 1.
    let o = env.crontab(&["-h"], None);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("Usage"), "{}", stderr(&o));
    assert!(stdout(&o).is_empty());
    let o = env.crond().arg("-h").output().unwrap();
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn syntax_test_matches_cronie_grammar() {
    let env = Env::new();
    for (line, ok) in [
        ("5/10 * * * * x", false),
        ("* * * * monday x", false),
        ("@HOURLY x", false),
        ("0~59/10 * * * * x", false),
        ("* * * * * -q x", false),
        ("-* * * * * x", nix::unistd::geteuid().is_root()),
        ("0 */5 * * * * cmd", false),
        ("5-3 * * * * x", true),
        ("* * * * sat-sun x", true),
        ("1FOO=bar", true),
        ("CRON_TZ=Mars/Olympus", true),
        ("RANDOM_DELAY=abc", true),
        ("* * * * * -n x", true),
    ] {
        let o = env.crontab(&["-T", "-"], Some(&format!("{line}\n")));
        assert_eq!(o.status.success(), ok, "{line:?}: {}", stderr(&o));
    }
    let o = env.crontab(&["-T", "-"], Some("*/61 * * * * x\n"));
    assert!(o.status.success());
    assert!(
        stderr(&o).contains("Warning: Step size 61 higher than possible maximum of 59"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn editor_nonzero_exit_is_reported() {
    let env = Env::new();
    let o = env
        .cmd(env!("CARGO_BIN_EXE_crontab"))
        .arg("-e")
        .env("VISUAL", "false")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(stderr(&o).contains("exited with"), "{}", stderr(&o));

    let o = env.crontab(&["-l"], None);
    assert!(!o.status.success());
    assert!(stderr(&o).contains(&format!("no crontab for {}", me())));
}

#[test]
fn install_from_nonexistent_path_fails() {
    let env = Env::new();
    let missing = env.path("does-not-exist");
    let o = env.crontab(&[missing.to_str().unwrap()], None);
    assert!(!o.status.success());
    assert!(
        stderr(&o).contains(missing.to_str().unwrap()),
        "{}",
        stderr(&o)
    );
}

#[test]
fn version_strings_mention_cronie() {
    let env = Env::new();
    let o = env
        .cmd(env!("CARGO_BIN_EXE_crontab"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(stdout(&o).contains("(cronie-compatible)"), "{}", stdout(&o));

    let o = env
        .cmd(env!("CARGO_BIN_EXE_crond"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(o.status.success());
    assert!(stdout(&o).contains("(cronie-compatible)"), "{}", stdout(&o));
}

#[test]
fn edit_temp_file_is_cleaned_up() {
    let env = Env::new();
    let tmp_dir = tempfile::tempdir().unwrap();
    let script = env.path("append.sh");
    fs::write(&script, "#!/bin/sh\necho '0 4 * * * appended' >> \"$1\"\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let o = env
        .cmd(env!("CARGO_BIN_EXE_crontab"))
        .arg("-e")
        .env("VISUAL", format!("/bin/sh {}", script.display()))
        .env("TMPDIR", tmp_dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", stderr(&o));

    let leftovers: Vec<_> = fs::read_dir(tmp_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("crontab."))
        .collect();
    assert!(leftovers.is_empty(), "{:?}", leftovers);
}

#[test]
fn crond_reboot_jobs_once_and_signals() {
    let env = Env::new();
    let out = env.path("booted");
    let o = env.crontab(
        &["-"],
        Some(&format!("@reboot echo up >> {}\n", out.display())),
    );
    assert!(o.status.success());

    let start = |env: &Env| {
        env.crond()
            .args(["-n", "-m", "off"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let stop = |child: &mut std::process::Child| {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "crond did not exit"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let mut child = start(&env);
    assert!(
        wait_for(&out, Duration::from_secs(10)),
        "@reboot job did not run"
    );
    assert!(wait_for(&env.path("crond.pid"), Duration::from_secs(5)));
    let pid: u32 = fs::read_to_string(env.path("crond.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(pid, child.id());

    // A second instance must refuse to start while the first holds the lock.
    let second = env.crond().args(["-n", "-m", "off"]).output().unwrap();
    assert!(!second.status.success());

    assert!(stop(&mut child).success());
    assert!(!env.path("crond.pid").exists());

    // Restart: marker exists, so @reboot does not run again.
    let mut child = start(&env);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(stop(&mut child).success());
    assert_eq!(fs::read_to_string(&out).unwrap(), "up\n");
}

#[test]
fn crond_logs_escaped_command_without_stdin_text() {
    let env = Env::new();
    let out = env.path("secret-out");
    fs::write(
        env.path("cron.d").join("secret"),
        format!("* * * * * {} cat > {}%hunter2\n", me(), out.display()),
    )
    .unwrap();
    let o = env
        .crond()
        .args(["-p", "-s", "--run-at", "2026-01-01 00:00"])
        .output()
        .unwrap();
    let err = stderr(&o);
    assert!(o.status.success(), "{err}");
    assert!(
        err.contains(&format!("CMD (cat > {})", out.display())),
        "{err}"
    );
    assert!(
        !err.contains("hunter2"),
        "stdin text must not be logged: {err}"
    );
    assert_eq!(fs::read_to_string(&out).unwrap(), "hunter2\n");
}

#[test]
fn syslog_output_only_with_s() {
    let env = Env::new();
    fs::write(
        env.path("cron.d").join("talk"),
        format!("* * * * * {} echo said-something\n", me()),
    )
    .unwrap();
    let o = env
        .crond()
        .args(["-p", "-s", "--run-at", "2026-01-01 00:00"])
        .output()
        .unwrap();
    assert!(
        stderr(&o).contains("CMDOUT (said-something)"),
        "{}",
        stderr(&o)
    );
    let o = env
        .crond()
        .args(["-p", "-m", "off", "--run-at", "2026-01-01 00:00"])
        .output()
        .unwrap();
    assert!(
        !stderr(&o).contains("CMDOUT"),
        "-m off discards output: {}",
        stderr(&o)
    );
}

#[test]
fn jobs_start_in_crontab_home_and_skip_when_chdir_fails() {
    let env = Env::new();
    let home = env.path("apphome");
    fs::create_dir(&home).unwrap();
    let out = env.path("pwd-out");
    let skipped = env.path("ran-without-home");
    fs::write(
        env.path("cron.d").join("home"),
        format!(
            "HOME={home}\n* * * * * {me} pwd > {out}\nHOME={home}/missing\n* * * * * {me} touch {skipped}\n",
            home = home.display(),
            me = me(),
            out = out.display(),
            skipped = skipped.display()
        ),
    )
    .unwrap();
    let o = env
        .crond()
        .args(["-p", "-s", "--run-at", "2026-01-01 00:00"])
        .output()
        .unwrap();
    let err = stderr(&o);
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        home.display().to_string()
    );
    assert!(!skipped.exists(), "{err}");
    assert!(err.contains("ERROR chdir failed ("), "{err}");
    assert!(!o.status.success());
}

#[test]
fn mail_follows_cronie_headers_and_safety_rules() {
    let env = Env::new();
    let mbox = env.path("mbox");
    fs::write(
        env.path("cron.d").join("mail"),
        format!(
            "MAILFROM=\"bad sender\"\nMAILTO=alice\n* * * * * {me} echo body-text\nMAILTO=\"eve;rm\"\n* * * * * {me} echo unsafe-mailto\n",
            me = me()
        ),
    )
    .unwrap();
    let o = env
        .crond()
        .args(["-p", "-m", &format!("cat >> '{}'", mbox.display())])
        .args(["--run-at", "2026-01-01 00:00"])
        .output()
        .unwrap();
    let err = stderr(&o);
    let mail = fs::read_to_string(&mbox).unwrap();
    assert!(
        mail.contains(&format!("From: \"(Cron Daemon)\" <{}>\n", me())),
        "an unsafe MAILFROM falls back to the user: {mail}"
    );
    assert!(mail.contains("To: alice\n"), "{mail}");
    assert!(mail.contains("\n\nbody-text\n"), "{mail}");
    assert!(!mail.contains("unsafe-mailto"), "{mail}");
    assert!(err.contains("UNSAFE (bad sender)"), "{err}");
}
