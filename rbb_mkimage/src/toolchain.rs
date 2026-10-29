//! Static-target compiler defaults, shared in behavior with the QEMU harness.
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn available(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|p| {
            p.join(name)
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

fn compiler(target: &str, host: &str, available: impl Fn(&str) -> bool) -> Option<String> {
    let arch = target.split('-').next()?;
    let mut candidates = vec![
        format!("{arch}-linux-musl-gcc"),
        format!("{arch}-unknown-linux-musl-gcc"),
    ];
    if host.split('-').next() == Some(arch) && host.contains("linux") {
        candidates.extend(["musl-gcc", "clang", "cc"].map(String::from));
    } else {
        candidates.extend([
            format!("{arch}-linux-gnu-gcc"),
            format!("{arch}-unknown-linux-gnu-gcc"),
        ]);
    }
    candidates.into_iter().find(|name| available(name))
}

fn rustc(args: &[&str]) -> io::Result<String> {
    let out = Command::new("rustc").args(args).output()?;
    if !out.status.success() {
        return Err(io::Error::other("cannot inspect Rust toolchain"));
    }
    String::from_utf8(out.stdout).map_err(io::Error::other)
}

pub fn configure(command: &mut Command, target: &str) -> io::Result<()> {
    if target.len() > 128
        || !target.contains("-linux-")
        || !target
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(io::Error::other(
            "target must be an installed Rust Linux triple",
        ));
    }
    let sysroot = rustc(&["--print", "sysroot"])?;
    let version = rustc(&["-vV"])?;
    let host = version
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .ok_or_else(|| io::Error::other("missing Rust host triple"))?;
    let linker_key = format!(
        "CARGO_TARGET_{}_LINKER",
        target.to_uppercase().replace('-', "_")
    );
    let linker = Path::new(sysroot.trim())
        .join("lib/rustlib")
        .join(host)
        .join("bin/rust-lld");
    if std::env::var_os(&linker_key).is_none() && linker.is_file() {
        command.env(linker_key, linker);
    }
    let cc_key = format!("CC_{}", target.replace('-', "_"));
    if std::env::var_os(&cc_key).is_none() && std::env::var_os("CC").is_none() {
        let cc = compiler(target, host, available).ok_or_else(|| {
            io::Error::other(format!("target C/assembly compiler required; set {cc_key}"))
        })?;
        command.env(cc_key, cc);
    }
    let ar_key = format!("AR_{}", target.replace('-', "_"));
    if std::env::var_os(&ar_key).is_none()
        && std::env::var_os("AR").is_none()
        && available("llvm-ar")
    {
        command.env(ar_key, "llvm-ar");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cross_target_never_falls_back_to_host_compiler() {
        assert_eq!(
            compiler(
                "x86_64-unknown-linux-musl",
                "x86_64-unknown-linux-gnu",
                |s| s == "clang"
            ),
            Some("clang".into())
        );
        assert_eq!(
            compiler(
                "aarch64-unknown-linux-musl",
                "x86_64-unknown-linux-gnu",
                |s| s == "clang"
            ),
            None
        );
        assert_eq!(
            compiler(
                "aarch64-unknown-linux-musl",
                "x86_64-unknown-linux-gnu",
                |s| s == "aarch64-linux-musl-gcc"
            ),
            Some("aarch64-linux-musl-gcc".into())
        );
    }
}
