mod shell;

use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::time::Duration;

pub const APPLETS: &[&str] = &[
    "init", "ctl", "sh", "echo", "cat", "ls", "mkdir", "cp", "mv", "rm", "ln", "readlink", "chmod",
    "pwd", "sleep", "true", "false", "uname", "mount", "umount", "insmod", "dmesg", "ps", "free",
    "kill", "poweroff", "reboot", "modprobe", "lsmod", "rmmod", "blkid",
];

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn cstring(text: &str) -> io::Result<CString> {
    CString::new(text).map_err(io::Error::other)
}
fn syscall(result: i32) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn exact(args: &[String], n: usize, usage: &str) -> io::Result<()> {
    if args.len() == n {
        Ok(())
    } else {
        Err(invalid(usage))
    }
}

pub fn entry(mut args: Vec<String>) -> i32 {
    if args.is_empty() {
        return 1;
    }
    let invoked = Path::new(&args[0])
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    args.remove(0);
    if args.first().is_some_and(|a| a == "--list") && !APPLETS.contains(&invoked.as_str()) {
        for applet in APPLETS {
            println!("{applet}");
        }
        return 0;
    }
    let name = if APPLETS.contains(&invoked.as_str()) {
        invoked
    } else if !args.is_empty() {
        args.remove(0)
    } else {
        eprintln!("rbb APPLET [ARGS...] | rbb --list");
        return 1;
    };
    if name == "init" {
        return rbb_init::main_entry(&args);
    }
    match run(&name, &args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{name}: {error}");
            1
        }
    }
}

pub fn run(name: &str, args: &[String]) -> io::Result<i32> {
    match name {
        "true" => {}
        "false" => return Ok(1),
        "sh" => return shell::run(args),
        "echo" => {
            let (newline, words) = if args.first().is_some_and(|a| a == "-n") {
                (false, &args[1..])
            } else {
                (true, args)
            };
            let mut out = io::stdout().lock();
            write!(
                out,
                "{}{}",
                words.join(" "),
                if newline { "\n" } else { "" }
            )?;
        }
        "cat" => {
            let mut out = io::stdout().lock();
            if args.is_empty() {
                io::copy(&mut io::stdin().lock(), &mut out)?;
            }
            for file in args {
                if file == "-" {
                    io::copy(&mut io::stdin().lock(), &mut out)?;
                } else {
                    io::copy(&mut fs::File::open(file)?, &mut out)?;
                }
            }
        }
        "pwd" => println!("{}", std::env::current_dir()?.display()),
        "ls" => {
            let paths = if args.is_empty() {
                vec![".".into()]
            } else {
                args.to_vec()
            };
            for path in paths {
                if !Path::new(&path).is_dir() {
                    println!("{path}");
                    fs::symlink_metadata(&path)?;
                    continue;
                }
                let mut entries: Vec<_> = fs::read_dir(path)?.collect::<Result<_, _>>()?;
                entries.sort_by_key(|e| e.file_name());
                for entry in entries {
                    println!("{}", entry.file_name().to_string_lossy());
                }
            }
        }
        "mkdir" => {
            let parents = args.first().is_some_and(|a| a == "-p");
            let paths = if parents { &args[1..] } else { args };
            if paths.is_empty() {
                return Err(invalid("mkdir [-p] PATH..."));
            }
            for path in paths {
                if parents {
                    fs::create_dir_all(path)?;
                } else {
                    fs::create_dir(path)?;
                }
            }
        }
        "cp" | "mv" => {
            exact(args, 2, "cp/mv SOURCE DEST")?;
            let dest = if Path::new(&args[1]).is_dir() {
                Path::new(&args[1]).join(
                    Path::new(&args[0])
                        .file_name()
                        .ok_or_else(|| invalid("invalid source"))?,
                )
            } else {
                Path::new(&args[1]).to_path_buf()
            };
            if name == "cp" {
                fs::copy(&args[0], dest)?;
            } else {
                fs::rename(&args[0], dest)?;
            }
        }
        "rm" => {
            if args.is_empty() {
                return Err(invalid("rm FILE... (no recursive removal)"));
            }
            for path in args {
                fs::remove_file(path)?;
            }
        }
        "ln" => {
            let symbolic = args.first().is_some_and(|a| a == "-s");
            let words = if symbolic { &args[1..] } else { args };
            exact(words, 2, "ln [-s] TARGET LINK")?;
            if symbolic {
                symlink(&words[0], &words[1])?;
            } else {
                fs::hard_link(&words[0], &words[1])?;
            }
        }
        "readlink" => {
            exact(args, 1, "readlink PATH")?;
            println!("{}", fs::read_link(&args[0])?.display());
        }
        "chmod" => {
            exact(args, 2, "chmod OCTAL PATH")?;
            let mode = u32::from_str_radix(&args[0], 8).map_err(io::Error::other)?;
            if mode > 0o7777 {
                return Err(invalid("mode out of range"));
            }
            fs::set_permissions(&args[1], fs::Permissions::from_mode(mode))?;
        }
        "sleep" => {
            exact(args, 1, "sleep SECONDS")?;
            let seconds: f64 = args[0].parse().map_err(io::Error::other)?;
            std::thread::sleep(Duration::try_from_secs_f64(seconds).map_err(io::Error::other)?);
        }
        "uname" => {
            if !args.is_empty() && args != ["-a"] && args != ["-r"] {
                return Err(invalid("uname [-a|-r]"));
            }
            let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
            syscall(unsafe { libc::uname(&mut uts) })?;
            let field = |v: &[libc::c_char]| {
                unsafe { std::ffi::CStr::from_ptr(v.as_ptr()) }.to_string_lossy()
            };
            if args == ["-r"] {
                println!("{}", field(&uts.release));
            } else if args == ["-a"] {
                println!(
                    "{} {} {} {} {}",
                    field(&uts.sysname),
                    field(&uts.nodename),
                    field(&uts.release),
                    field(&uts.version),
                    field(&uts.machine)
                );
            } else {
                println!("{}", field(&uts.sysname));
            }
        }
        "mount" => {
            if args.is_empty() {
                print!("{}", fs::read_to_string("/proc/mounts")?);
                return Ok(0);
            }
            let mut kind = None;
            let mut flags = 0;
            let mut data = Vec::new();
            let mut paths = Vec::new();
            let mut iter = args.iter();
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "-t" => {
                        kind = Some(
                            iter.next()
                                .ok_or_else(|| invalid("-t requires a filesystem"))?
                                .as_str(),
                        )
                    }
                    "-r" => flags |= libc::MS_RDONLY,
                    "-o" => {
                        for option in iter
                            .next()
                            .ok_or_else(|| invalid("-o requires options"))?
                            .split(',')
                        {
                            match option {
                                "ro" => flags |= libc::MS_RDONLY,
                                "rw" => flags &= !libc::MS_RDONLY,
                                "nosuid" => flags |= libc::MS_NOSUID,
                                "nodev" => flags |= libc::MS_NODEV,
                                "noexec" => flags |= libc::MS_NOEXEC,
                                "bind" => flags |= libc::MS_BIND,
                                "remount" => flags |= libc::MS_REMOUNT,
                                "defaults" => {}
                                _ => data.push(option),
                            }
                        }
                    }
                    _ if arg.starts_with('-') => return Err(invalid("unknown mount option")),
                    _ => paths.push(arg),
                }
            }
            if paths.len() != 2 {
                return Err(invalid("mount -t TYPE [-r] [-o OPTIONS] SOURCE TARGET"));
            }
            let kind = match kind {
                Some(kind) => kind,
                None if flags & libc::MS_BIND != 0 => "",
                None => return Err(invalid("mount requires -t")),
            };
            rbb_init::mount(paths[0], paths[1], kind, flags, &data.join(","))?;
        }
        "umount" => {
            exact(args, 1, "umount TARGET")?;
            syscall(unsafe { libc::umount2(cstring(&args[0])?.as_ptr(), 0) })?;
        }
        "insmod" => {
            let module = args
                .first()
                .ok_or_else(|| invalid("insmod MODULE.ko[.gz|.xz] [PARAMETERS...]"))?;
            rbb_boot::modules::insert(Path::new(module), &args[1..].join(" "))?;
        }
        "modprobe" => {
            let module = args
                .first()
                .ok_or_else(|| invalid("modprobe MODULE [PARAMETERS...]"))?;
            rbb_boot::modules::load(module, &args[1..].join(" "))?;
        }
        "lsmod" => {
            exact(args, 0, "lsmod")?;
            print!("{}", fs::read_to_string("/proc/modules")?);
        }
        "rmmod" => {
            exact(args, 1, "rmmod MODULE")?;
            let module = cstring(&args[0].replace('-', "_"))?;
            if unsafe { libc::syscall(libc::SYS_delete_module, module.as_ptr(), libc::O_NONBLOCK) }
                == -1
            {
                return Err(io::Error::last_os_error());
            }
        }
        "blkid" => {
            let devices = if args.is_empty() {
                rbb_boot::device::discover()?
            } else {
                args.iter()
                    .map(|p| rbb_boot::device::probe(Path::new(p)))
                    .collect::<io::Result<Vec<_>>>()?
            };
            for device in devices {
                if device.fstype.is_none() && device.partuuid.is_none() {
                    continue;
                }
                print!("{}:", device.path.display());
                for (key, value) in [
                    ("TYPE", device.fstype),
                    ("UUID", device.uuid),
                    ("LABEL", device.label),
                    ("PARTUUID", device.partuuid),
                ] {
                    if let Some(value) = value {
                        print!(" {key}={value:?}");
                    }
                }
                println!();
            }
        }
        "dmesg" => {
            exact(args, 0, "dmesg (read-only)")?;
            let size = unsafe { libc::klogctl(10, std::ptr::null_mut(), 0) };
            if size == -1 {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = vec![0u8; size as usize];
            let count = unsafe { libc::klogctl(3, buffer.as_mut_ptr().cast(), size) };
            if count == -1 {
                return Err(io::Error::last_os_error());
            }
            io::stdout().write_all(&buffer[..count as usize])?;
        }
        "free" => print!("{}", fs::read_to_string("/proc/meminfo")?),
        "ps" => {
            println!("PID\tNAME\tSTATE");
            let mut pids: Vec<u32> = fs::read_dir("/proc")?
                .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .collect();
            pids.sort_unstable();
            for pid in pids {
                if let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) {
                    let get = |key: &str| {
                        status
                            .lines()
                            .find_map(|s| s.strip_prefix(key))
                            .unwrap_or("")
                            .trim()
                    };
                    println!("{pid}\t{}\t{}", get("Name:"), get("State:"));
                }
            }
        }
        "kill" => {
            let (signal, pids) = if args.first().is_some_and(|a| a.starts_with('-')) {
                let signal = args[0][1..].parse::<i32>().map_err(io::Error::other)?;
                (signal, &args[1..])
            } else {
                (libc::SIGTERM, args)
            };
            if pids.is_empty() {
                return Err(invalid("kill [-SIGNAL_NUMBER] PID..."));
            }
            for pid in pids {
                syscall(unsafe { libc::kill(pid.parse().map_err(io::Error::other)?, signal) })?;
            }
        }
        "ctl" | "poweroff" | "reboot" => {
            let mut path = Path::new("/run/rbb/control");
            let mut words = args;
            if args.first().is_some_and(|v| v == "--socket") {
                path = Path::new(args.get(1).ok_or_else(|| invalid("--socket needs PATH"))?);
                words = &args[2..];
            }
            let action = vec![name.to_string()];
            if name != "ctl" && !words.is_empty() {
                return Err(invalid("poweroff/reboot [--socket PATH]"));
            }
            print!(
                "{}",
                rbb_init::control(path, if name == "ctl" { words } else { &action })?
            );
        }
        _ => return Err(invalid(&format!("unknown applet {name}; use rbb --list"))),
    }
    Ok(0)
}
