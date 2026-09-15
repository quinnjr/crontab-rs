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
    assert!(stderr(&o).contains("INSECURE MODE") || stderr(&o).contains("WRONG FILE OWNER"));
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
