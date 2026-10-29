//! VM test payload only; not included in normal images or APPLETS.
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn check(condition: bool, message: &str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message))
    }
}
fn wait(mut predicate: impl FnMut() -> bool, message: &str) -> io::Result<()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if predicate() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    Err(io::Error::other(format!("timeout: {message}")))
}
fn applet(name: &str, args: &[&str]) -> io::Result<String> {
    let output = Command::new(format!("/bin/{name}")).args(args).output()?;
    check(
        output.status.success(),
        &format!("{name} failed: {}", String::from_utf8_lossy(&output.stderr)),
    )?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
fn status() -> String {
    applet("ctl", &["status"]).unwrap_or_default()
}
fn line(text: &str, service: &str) -> String {
    text.lines()
        .find(|l| l.starts_with(&format!("{service} ")))
        .unwrap_or("")
        .to_string()
}
fn count(path: &str) -> u32 {
    fs::read_to_string(path)
        .unwrap_or_default()
        .parse()
        .unwrap_or(0)
}
fn bump(path: &str) -> io::Result<u32> {
    let value = count(path) + 1;
    fs::write(path, value.to_string())?;
    Ok(value)
}
fn forever() -> ! {
    loop {
        unsafe {
            libc::pause();
        }
    }
}

fn verify() -> io::Result<()> {
    check(unsafe { libc::getppid() } == 1, "supervisor is not PID 1")?;
    for (path, kind) in [
        ("/proc", "proc"),
        ("/sys", "sysfs"),
        ("/dev", "devtmpfs"),
        ("/run", "tmpfs"),
        ("/tmp", "tmpfs"),
        ("/dev/pts", "devpts"),
    ] {
        let mounts = fs::read_to_string("/proc/mounts")?;
        check(
            mounts
                .lines()
                .any(|l| l.contains(&format!(" {path} {kind} "))),
            &format!("missing mount {path}"),
        )?;
    }
    check(
        fs::read_to_string("/run/setup")? == "ready",
        "oneshot ordering / env / cwd",
    )?;
    wait(|| count("/run/crash-count") >= 2, "crashed service restart")?;
    wait(
        || line(&status(), "retry").contains("state=complete"),
        "retry oneshot completion",
    )?;
    check(
        fs::read_to_string("/run/retry-count")? == "2",
        "oneshot retry policy",
    )?;
    wait(
        || {
            let pid = count("/run/orphan-pid");
            pid > 0 && !Path::new(&format!("/proc/{pid}")).exists()
        },
        "adopted orphan reaping",
    )?;
    let initial = status();
    for (service, expected) in [
        ("setup", "complete"),
        ("failure", "failed"),
        ("missing", "failed"),
        ("disabled", "down"),
        ("blocked", "waiting"),
    ] {
        check(
            line(&initial, service).contains(&format!("state={expected}")),
            &format!("unexpected {service} status: {initial}"),
        )?;
    }
    check(
        !Path::new("/run/should-not-run").exists(),
        "disabled or blocked service ran",
    )?;
    let original_pid = line(&initial, "crash")
        .split_whitespace()
        .find_map(|v| v.strip_prefix("pid="))
        .unwrap_or("0")
        .to_string();
    applet("ctl", &["up", "crash"])?; // up on an already-running service is idempotent
    check(
        line(&status(), "crash").contains(&format!("pid={original_pid} ")),
        "up restarted running service",
    )?;
    applet("ctl", &["down", "crash"])?;
    wait(
        || line(&status(), "crash").contains("state=down pid=0"),
        "service down",
    )?;
    let stopped = count("/run/crash-count");
    std::thread::sleep(Duration::from_millis(350));
    check(
        count("/run/crash-count") == stopped,
        "down service restarted",
    )?;
    applet("ctl", &["up", "crash"])?;
    wait(|| count("/run/crash-count") > stopped, "service up")?;
    let started = count("/run/crash-count");
    applet("ctl", &["restart", "crash"])?;
    wait(
        || count("/run/crash-count") > started,
        "controlled service restart",
    )?;
    // Per-service TERM deadline, not just global shutdown deadline.
    applet("ctl", &["down", "stubborn"])?;
    wait(
        || line(&status(), "stubborn").contains("state=down pid=0"),
        "SIGKILL escalation on down",
    )?;
    applet("ctl", &["up", "stubborn"])?;
    wait(
        || line(&status(), "stubborn").contains("state=up"),
        "stubborn restarted",
    )?;
    wait(
        || {
            let pid = count("/run/stubborn-pid");
            fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|s| s.contains("SigIgn:"))
        },
        "stubborn initialization",
    )?;
    check(
        applet("echo", &["hello", "world"])? == "hello world\n",
        "echo",
    )?;
    let direct = Command::new("/bbin/rbb")
        .args(["echo", "direct"])
        .output()?;
    check(
        direct.status.success() && direct.stdout == b"direct\n",
        "multicall subcommand dispatch",
    )?;
    check(
        applet("sh", &["-c", "echo 'quoted text' \"\""])? == "quoted text \n",
        "command interpreter quoting",
    )?;
    applet("mkdir", &["-p", "/tmp/a/b"])?;
    fs::write("/tmp/a/source", "payload\n")?;
    applet("cp", &["/tmp/a/source", "/tmp/a/b/copy"])?;
    applet("ln", &["-s", "b/copy", "/tmp/a/link"])?;
    check(
        applet("cat", &["/tmp/a/link"])? == "payload\n",
        "copy / symlink / cat",
    )?;
    check(
        applet("readlink", &["/tmp/a/link"])? == "b/copy\n",
        "readlink",
    )?;
    applet("chmod", &["600", "/tmp/a/source"])?;
    applet("mv", &["/tmp/a/source", "/tmp/a/moved"])?;
    check(
        applet("ls", &["/tmp/a"])? == "b\nlink\nmoved\n",
        "sorted ls",
    )?;
    applet("rm", &["/tmp/a/moved"])?;
    check(applet("uname", &["-a"])?.contains("Linux"), "uname")?;
    check(applet("free", &[])?.contains("MemTotal:"), "free")?;
    check(
        applet("ps", &[])?.contains("\n1\tinit\t"),
        "ps does not list PID 1",
    )?;
    applet("dmesg", &[])?;
    applet("mkdir", &["/tmp/test-mount"])?;
    applet(
        "mount",
        &[
            "-t",
            "tmpfs",
            "-o",
            "nosuid,nodev,mode=0700",
            "tmpfs",
            "/tmp/test-mount",
        ],
    )?;
    check(
        fs::read_to_string("/proc/mounts")?.contains(" /tmp/test-mount tmpfs "),
        "mount utility",
    )?;
    applet("umount", &["/tmp/test-mount"])?;
    println!("RBB_GUEST_TEST_PASS");
    io::stdout().flush()?;
    let action = fs::read_to_string("/etc/rbb/test-action").unwrap_or_else(|_| "poweroff".into());
    applet("ctl", &[action.trim()])?;
    Ok(())
}

fn run(role: &str) -> io::Result<i32> {
    match role {
        "setup" => {
            check(
                std::env::var("RBB_TEST").as_deref() == Ok("works"),
                "environment not passed",
            )?;
            check(
                std::env::current_dir()? == Path::new("/tmp"),
                "cwd not applied",
            )?;
            fs::write("/run/setup", "ready")?;
        }
        "crash" => {
            check(Path::new("/run/setup").exists(), "dependency not completed")?;
            if bump("/run/crash-count")? == 1 {
                return Ok(23);
            }
            forever();
        }
        "retry" => {
            if bump("/run/retry-count")? == 1 {
                return Ok(24);
            }
        }
        "stubborn" => {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            fs::write("/run/stubborn-pid", std::process::id().to_string())?;
            forever();
        }
        "orphan" => {
            let pid = unsafe { libc::fork() };
            if pid == -1 {
                return Err(io::Error::last_os_error());
            }
            if pid == 0 {
                // Escape the leader's process group so leader-exit cleanup does
                // not kill us. PID 1 must adopt and reap us after our own exit.
                unsafe {
                    libc::setsid();
                }
                fs::write("/run/orphan-pid", std::process::id().to_string())?;
                std::thread::sleep(Duration::from_millis(300));
                unsafe {
                    libc::_exit(0);
                }
            }
            wait(|| Path::new("/run/orphan-pid").exists(), "orphan setsid")?;
        }
        "should-not-run" => {
            fs::write("/run/should-not-run", "bad")?;
        }
        "verify" => verify()?,
        _ => return Err(io::Error::other("unknown guest test role")),
    }
    Ok(0)
}
fn main() {
    match run(&std::env::args().nth(1).unwrap_or_default()) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("RBB_GUEST_TEST_FAIL: {error}");
            // Keep PID 1 alive; host harness will enforce a bounded timeout.
            std::process::exit(1);
        }
    }
}
