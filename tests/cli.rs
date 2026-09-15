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

    // Install from a file path, adding a missing trailing newline.
    let file = env.path("mytab");
    fs::write(&file, "@daily true").unwrap();
    let o = env.crontab(&[file.to_str().unwrap()], None);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&env.crontab(&["-l"], None)), "@daily true\n");

    // -u for yourself is allowed.
    let o = env.crontab(&["-u", &me(), "-l"], None);
    assert!(o.status.success());

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
    assert!(stdout(&o).contains("No syntax issues"));
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
        .env("VISUAL", &script)
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
        .env("VISUAL", &bad)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"n\n").unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(!o.status.success());
    assert!(stderr(&o).contains("bad minute"));
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
        stderr(&o).contains("must be privileged to set the cluster host"),
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
fn crond_rejects_system_crontab_with_unknown_user() {
    let env = Env::new();
    let out = env.path("ran");
    fs::write(
        env.path("cron.d").join("badjob"),
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
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stderr(&o).contains("BAD CRONTAB"), "{}", stderr(&o));
    assert!(stderr(&o).contains("bad username"), "{}", stderr(&o));
    assert!(!out.exists(), "no job from a rejected file may run");
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
    let o = env.crontab(&["--help"], None);
    assert_eq!(o.status.code(), Some(0));
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
        ("-* * * * * x", false),
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
        .env("VISUAL", &script)
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
