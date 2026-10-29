//! Linux PID 1 / subreaper with foreground services and a local control socket.
//! `after` is startup ordering, not readiness notification or dependency liveness.
pub mod config;
mod supervisor;

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;

pub use supervisor::{control, supervise};

pub fn mount(
    source: &str,
    target: &str,
    kind: &str,
    flags: libc::c_ulong,
    data: &str,
) -> io::Result<()> {
    let c = |s: &str| CString::new(s).map_err(io::Error::other);
    let (source, target, kind, data) = (c(source)?, c(target)?, c(kind)?, c(data)?);
    let result = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            kind.as_ptr(),
            flags,
            data.as_ptr().cast(),
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn boot_filesystems() -> io::Result<()> {
    for (path, kind, flags, data) in [
        ("/dev", "devtmpfs", libc::MS_NOSUID, "mode=0755"),
        (
            "/proc",
            "proc",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            "",
        ),
        (
            "/sys",
            "sysfs",
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            "",
        ),
        (
            "/run",
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            "mode=0755",
        ),
        (
            "/tmp",
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            "mode=1777",
        ),
        (
            "/dev/pts",
            "devpts",
            libc::MS_NOSUID | libc::MS_NOEXEC,
            "mode=0620,ptmxmode=0666",
        ),
    ] {
        fs::create_dir_all(path)?;
        if let Err(e) = mount(kind, path, kind, flags, data) {
            if e.raw_os_error() != Some(libc::EBUSY) {
                return Err(io::Error::new(e.kind(), format!("mount {path}: {e}")));
            }
        }
    }
    for (name, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        let path = format!("/dev/{name}");
        if !std::path::Path::new(&path).exists() {
            symlink(target, path)?;
        }
    }
    Ok(())
}

pub fn run(args: &[String]) -> io::Result<()> {
    let mut config_path = "/etc/rbb/init.toml".to_string();
    let mut standalone = false;
    let mut check = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_path = args
                    .next()
                    .ok_or_else(|| io::Error::other("--config needs a path"))?
                    .clone()
            }
            "--supervise" => standalone = true,
            "--check" => check = true,
            "--help" => {
                println!("rbb-init [--config PATH] [--check | --supervise]\nWithout --supervise, must run as PID 1. --supervise never mounts or reboots.");
                return Ok(());
            }
            _ => return Err(io::Error::other(format!("unknown init argument {arg}"))),
        }
    }
    let config = config::Config::parse(&fs::read_to_string(config_path)?)?;
    if check {
        return Ok(());
    }
    let pid1 = unsafe { libc::getpid() } == 1;
    if !pid1 && !standalone {
        return Err(io::Error::other(
            "init must be PID 1; use --supervise for host testing",
        ));
    }
    if pid1 && !standalone {
        boot_filesystems()?;
        let boot = (|| {
            let cmdline = rbb_boot::cmdline::Cmdline::parse(&fs::read_to_string("/proc/cmdline")?)?;
            for module in config.modules.iter().map(String::as_str).chain(
                cmdline
                    .get("rbb.modules")
                    .unwrap_or("")
                    .split(',')
                    .filter(|m| !m.is_empty()),
            ) {
                rbb_boot::modules::load(module, "")?;
            }
            if let Some(root) = rbb_boot::RootConfig::from_cmdline(&cmdline, config.root.clone())? {
                if let Some(delay) = cmdline.get("rootdelay") {
                    let seconds: u64 = delay.parse().map_err(io::Error::other)?;
                    if seconds > 300 {
                        return Err(io::Error::other("rootdelay exceeds 300 seconds"));
                    }
                    std::thread::sleep(std::time::Duration::from_secs(seconds));
                }
                return rbb_boot::root::PreparedRoot::prepare(&root).map(Some);
            }
            Ok(None)
        })();
        match boot {
            // After this point a mount-move failure is fatal, not recoverable
            // by starting services against a potentially half-switched root.
            Ok(Some(root)) => return root.exec(),
            Ok(None) => {}
            Err(error) => {
                eprintln!("rbb-init: boot preparation failed: {error}; entering configured rescue services");
                fs::create_dir_all("/run/rbb")?;
                fs::write("/run/rbb/boot-error", error.to_string())?;
            }
        }
    }
    supervise(config, pid1 && !standalone)
}

/// PID 1 must never return to the kernel, even after an unrecoverable boot error.
pub fn main_entry(args: &[String]) -> i32 {
    let code = match run(args) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("rbb-init: {error}");
            1
        }
    };
    if unsafe { libc::getpid() } == 1 {
        eprintln!("rbb-init: init returned; staying alive (check init.toml / console)");
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    code
}
