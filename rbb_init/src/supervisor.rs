use crate::config::{Config, Kind, Restart};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

struct Signals {
    fd: OwnedFd,
    old: libc::sigset_t,
}
impl Signals {
    fn new() -> io::Result<Self> {
        unsafe {
            let mut set = std::mem::zeroed();
            let mut old = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for signal in [
                libc::SIGCHLD,
                libc::SIGTERM,
                libc::SIGINT,
                libc::SIGUSR1,
                libc::SIGUSR2,
                libc::SIGHUP,
            ] {
                libc::sigaddset(&mut set, signal);
            }
            let error = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
            if fd == -1 {
                libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                fd: OwnedFd::from_raw_fd(fd),
                old,
            })
        }
    }
    fn drain(&self) -> Vec<i32> {
        let mut signals = Vec::new();
        unsafe {
            let mut info: libc::signalfd_siginfo = std::mem::zeroed();
            while libc::read(
                self.fd.as_raw_fd(),
                (&mut info as *mut libc::signalfd_siginfo).cast(),
                std::mem::size_of_val(&info),
            ) > 0
            {
                signals.push(info.ssi_signo as i32);
            }
        }
        signals
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.old, std::ptr::null_mut());
        }
    }
}

struct SocketGuard(String);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct State {
    pid: Option<i32>,
    desired: bool,
    done: bool,
    ready: bool,
    attempts: u64,
    last: Option<i32>,
    next: Instant,
    stopping: Option<Instant>,
}

fn signal_group(pid: i32, signal: i32) {
    unsafe {
        libc::kill(-pid, signal);
    }
}

/// Run on the main thread: signals are blocked here and restored before exec.
/// Services must remain in the foreground and must not daemonize / escape groups.
pub fn supervise(config: Config, reboot_host: bool) -> io::Result<()> {
    config.validate()?;
    let order = config.order()?;
    let signals = Signals::new()?;
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if let Some(parent) = Path::new(&config.control).parent() {
        fs::create_dir_all(parent)?;
    }
    // Refuse an existing socket rather than steal another supervisor's endpoint.
    let socket = UnixDatagram::bind(&config.control)?;
    let _guard = SocketGuard(config.control.clone());
    fs::set_permissions(&config.control, fs::Permissions::from_mode(0o600))?;
    socket.set_nonblocking(true)?;
    let mut states: Vec<State> = config
        .services
        .iter()
        .map(|s| State {
            pid: None,
            desired: !s.disabled,
            done: false,
            ready: false,
            attempts: 0,
            last: None,
            next: Instant::now(),
            stopping: None,
        })
        .collect();
    let mut shutdown: Option<(Instant, bool)> = None;
    let mut killed = false;
    eprintln!("rbb-init: supervising {} services", states.len());
    loop {
        for signal in signals.drain() {
            match signal {
                libc::SIGTERM | libc::SIGUSR1 => {
                    shutdown.get_or_insert((Instant::now(), false));
                }
                libc::SIGINT | libc::SIGUSR2 => {
                    shutdown.get_or_insert((Instant::now(), true));
                }
                _ => {} // SIGCHLD is drained by waitpid; HUP deliberately does not reload.
            }
        }
        // Reap *all* children, including adopted orphans, not just service leaders.
        let no_children = loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break pid == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
            }
            if let Some(i) = states.iter().position(|s| s.pid == Some(pid)) {
                let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
                let code = if libc::WIFEXITED(status) {
                    libc::WEXITSTATUS(status)
                } else {
                    128 + libc::WTERMSIG(status)
                };
                // Kill descendants left behind by an exited leader before a restart.
                signal_group(pid, libc::SIGKILL);
                let state = &mut states[i];
                state.pid = None;
                state.ready = config.services[i].kind == Kind::Oneshot && success;
                state.last = Some(code);
                let controlled = state.stopping.take().is_some();
                state.done = !controlled
                    && match config.services[i].policy() {
                        Restart::Never => true,
                        Restart::OnFailure => success,
                        Restart::Always => false,
                    };
                state.next =
                    Instant::now() + Duration::from_millis(config.services[i].restart_delay_ms);
                eprintln!("rbb-init: {} exited {code}", config.services[i].name);
            }
        };
        // Limit commands per tick so a busy client cannot starve child reaping.
        for _ in 0..32 {
            let mut buffer = [0u8; 4096];
            let (length, peer) = match socket.recv_from(&mut buffer) {
                Ok(value) => value,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            };
            let text = String::from_utf8_lossy(&buffer[..length]);
            let words: Vec<_> = text.split_whitespace().collect();
            let response = match words.as_slice() {
                ["status"] => config
                    .services
                    .iter()
                    .zip(&states)
                    .map(|(s, st)| {
                        format!(
                            "{} state={} pid={} attempts={} exit={}\n",
                            s.name,
                            if st.stopping.is_some() {
                                "stopping"
                            } else if st.pid.is_some() {
                                "up"
                            } else if !st.desired {
                                "down"
                            } else if st.done && st.ready {
                                "complete"
                            } else if st.done {
                                "failed"
                            } else {
                                "waiting"
                            },
                            st.pid.unwrap_or(0),
                            st.attempts,
                            st.last.map(|c| c.to_string()).unwrap_or_else(|| "-".into())
                        )
                    })
                    .collect::<String>(),
                ["poweroff"] | ["reboot"] => {
                    shutdown.get_or_insert((Instant::now(), words[0] == "reboot"));
                    "ok\n".into()
                }
                [verb @ ("up" | "down" | "restart"), name] if shutdown.is_none() => {
                    if let Some(i) = config.services.iter().position(|s| s.name == *name) {
                        let state = &mut states[i];
                        state.desired = *verb != "down";
                        if *verb != "up" || state.pid.is_none() {
                            state.done = false;
                            state.ready = false;
                            state.next = Instant::now();
                        }
                        if *verb != "up" {
                            if let Some(pid) = state.pid {
                                signal_group(pid, libc::SIGTERM);
                                state.stopping = Some(
                                    Instant::now()
                                        + Duration::from_millis(config.shutdown_timeout_ms),
                                );
                            }
                        }
                        "ok\n".into()
                    } else {
                        format!("error: unknown service {name}\n")
                    }
                }
                _ => "error: expected status | up/down/restart NAME | poweroff | reboot\n".into(),
            };
            if let Some(path) = peer.as_pathname() {
                let _ = socket.send_to(response.as_bytes(), path);
            }
        }
        if let Some((start, reboot)) = shutdown {
            if states.iter().any(|s| s.desired) {
                eprintln!("rbb-init: shutdown requested");
                for &i in order.iter().rev() {
                    states[i].desired = false;
                    if let Some(pid) = states[i].pid {
                        signal_group(pid, libc::SIGTERM);
                    }
                }
                // PID 1 owns the entire namespace, including escaped orphans.
                if reboot_host {
                    unsafe {
                        libc::kill(-1, libc::SIGTERM);
                    }
                }
            }
            if !killed && start.elapsed() >= Duration::from_millis(config.shutdown_timeout_ms) {
                for state in &states {
                    if let Some(pid) = state.pid {
                        signal_group(pid, libc::SIGKILL);
                    }
                }
                if reboot_host {
                    unsafe {
                        libc::kill(-1, libc::SIGKILL);
                    }
                }
                killed = true;
                eprintln!("rbb-init: shutdown deadline; sent SIGKILL");
            }
            if no_children
                || (killed
                    && start.elapsed() > Duration::from_millis(config.shutdown_timeout_ms + 1000))
            {
                eprintln!("rbb-init: shutdown complete");
                if reboot_host {
                    unsafe {
                        libc::sync();
                        libc::reboot(if reboot {
                            libc::RB_AUTOBOOT
                        } else {
                            libc::RB_POWER_OFF
                        });
                    }
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
        } else {
            for &i in &order {
                if let (Some(pid), Some(deadline)) = (states[i].pid, states[i].stopping) {
                    if Instant::now() >= deadline {
                        signal_group(pid, libc::SIGKILL);
                    }
                }
                if !states[i].desired
                    || states[i].done
                    || states[i].pid.is_some()
                    || Instant::now() < states[i].next
                {
                    continue;
                }
                if !config.services[i].after.iter().all(|name| {
                    let index = config
                        .services
                        .iter()
                        .position(|s| &s.name == name)
                        .unwrap();
                    states[index].ready
                }) {
                    continue;
                }
                let service = &config.services[i];
                let mut command = Command::new(&service.command[0]);
                command
                    .args(&service.command[1..])
                    .envs(&service.env)
                    .env("PATH", "/bbin:/bin:/sbin:/usr/bin");
                if let Some(cwd) = &service.cwd {
                    command.current_dir(cwd);
                }
                let mask = signals.old;
                unsafe {
                    command.pre_exec(move || {
                        if libc::setpgid(0, 0) == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        let error =
                            libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
                        if error != 0 {
                            return Err(io::Error::from_raw_os_error(error));
                        }
                        Ok(())
                    });
                }
                let state = &mut states[i];
                state.attempts += 1;
                match command.spawn() {
                    Ok(child) => {
                        state.pid = Some(child.id() as i32);
                        state.ready = service.kind == Kind::Longrun;
                        eprintln!("rbb-init: started {} pid={}", service.name, child.id());
                    }
                    Err(error) => {
                        state.last = Some(127);
                        state.done = service.policy() == Restart::Never;
                        state.next =
                            Instant::now() + Duration::from_millis(service.restart_delay_ms);
                        eprintln!("rbb-init: {}: {error}", service.name);
                    }
                }
            }
        }
        let mut fds = [
            libc::pollfd {
                fd: signals.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        unsafe {
            libc::poll(fds.as_mut_ptr(), fds.len() as _, 50);
        }
    }
}

pub fn control(path: &Path, words: &[String]) -> io::Result<String> {
    // A private directory prevents another user spoofing the response endpoint.
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!("rbb-ctl-{}-{serial}", std::process::id()));
    fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let result = (|| {
        let socket = UnixDatagram::bind(directory.join("reply"))?;
        socket.set_read_timeout(Some(Duration::from_secs(3)))?;
        socket.connect(path)?;
        socket.send(words.join(" ").as_bytes())?;
        let mut buffer = [0u8; 65536];
        let length = socket.recv(&mut buffer)?;
        let response = String::from_utf8_lossy(&buffer[..length]).into_owned();
        if response.starts_with("error:") {
            Err(io::Error::other(response))
        } else {
            Ok(response)
        }
    })();
    let _ = fs::remove_dir_all(directory);
    result
}

use std::os::unix::fs::DirBuilderExt;
