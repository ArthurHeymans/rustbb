use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Supervisor {
    child: Child,
    directory: tempfile::TempDir,
    socket: PathBuf,
}
impl Supervisor {
    fn ctl(&self, command: &[&str]) -> String {
        rbb_init::control(
            &self.socket,
            &command.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .unwrap()
    }
    fn status(&self) -> String {
        self.ctl(&["status"])
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        // Also clean up service groups if an assertion panics mid-test.
        if let Ok(contents) = fs::read_to_string(self.directory.path().join("daemon.pid")) {
            if let Ok(pid) = contents.trim().parse::<i32>() {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn service_line(status: &str, name: &str) -> String {
    status
        .lines()
        .find(|l| l.starts_with(&format!("{name} ")))
        .unwrap()
        .into()
}
fn pid(status: &str, name: &str) -> i32 {
    service_line(status, name)
        .split_whitespace()
        .find_map(|s| s.strip_prefix("pid="))
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn host_supervision_and_bounded_shutdown() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("control");
    let pidfile = directory.path().join("daemon.pid");
    let config_path = directory.path().join("init.toml");
    let config = format!(
        r#"
        control = {socket:?}
        shutdown_timeout_ms = 100
        [[services]]
        name = "setup"
        kind = "oneshot"
        command = ["/bin/sh", "-c", "exit 0"]
        [[services]]
        name = "daemon"
        command = ["/bin/sh", "-c", "trap '' TERM; echo $$ > {pidfile}; while :; do :; done"]
        after = ["setup"]
        restart_delay_ms = 50
        [[services]]
        name = "bad"
        kind = "oneshot"
        command = ["/bin/sh", "-c", "exit 7"]
        [[services]]
        name = "blocked"
        command = ["/not-an-executable"]
        after = ["bad"]
        [[services]]
        name = "disabled"
        command = ["/not-an-executable"]
        disabled = true
    "#,
        socket = socket.display().to_string(),
        pidfile = pidfile.display()
    );
    fs::write(&config_path, config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_rbb-init"))
        .args(["--supervise", "--config"])
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut supervisor = Supervisor {
        child,
        directory,
        socket,
    };
    wait(|| supervisor.socket.exists());
    wait(|| pidfile.exists() && supervisor.status().contains("bad state=failed"));
    let initial = supervisor.status();
    assert!(initial.contains("setup state=complete"), "{initial}");
    assert!(
        initial.contains("blocked state=waiting pid=0 attempts=0"),
        "{initial}"
    );
    assert!(
        initial.contains("disabled state=down pid=0 attempts=0"),
        "{initial}"
    );
    let first = pid(&initial, "daemon");
    assert!(first > 0);
    assert_eq!(supervisor.ctl(&["up", "daemon"]), "ok\n");
    assert_eq!(pid(&supervisor.status(), "daemon"), first);
    unsafe {
        libc::kill(first, libc::SIGKILL);
    }
    wait(|| {
        let new = pid(&supervisor.status(), "daemon");
        new != first && new > 0
    });
    assert_eq!(supervisor.ctl(&["down", "daemon"]), "ok\n");
    wait(|| supervisor.status().contains("daemon state=down pid=0"));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(pid(&supervisor.status(), "daemon"), 0);
    supervisor.ctl(&["up", "daemon"]);
    wait(|| pid(&supervisor.status(), "daemon") > 0);
    let old = pid(&supervisor.status(), "daemon");
    // Ensure the shell has installed its ignored-TERM disposition.
    wait(|| fs::read_to_string(&pidfile).is_ok_and(|s| s.trim() == old.to_string()));
    let start = Instant::now();
    supervisor.ctl(&["poweroff"]);
    wait(|| supervisor.child.try_wait().unwrap().is_some());
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(!Path::new(&format!("/proc/{old}")).exists());
    assert!(!supervisor.socket.exists());
    assert!(supervisor.child.wait().unwrap().success());
}

#[test]
fn invalid_config_is_rejected_before_boot() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("invalid.toml");
    fs::write(
        &config,
        "[[services]]\nname='cycle'\ncommand=['/x']\nafter=['cycle']",
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_rbb-init"))
        .args(["--check", "--config"])
        .arg(config)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("cycle"));
}
