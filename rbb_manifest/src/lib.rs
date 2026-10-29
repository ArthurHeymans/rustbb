//! The command inventory is shared, but packaging reads the target ELF's copy.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io;

pub const SECTION: &str = ".rbb.commands";
pub const COREUTILS_EXTRA: &[&str] = &[
    "head",
    "tail",
    "wc",
    "cut",
    "sort",
    "uniq",
    "tee",
    "tr",
    "env",
    "printenv",
    "mktemp",
    "timeout",
    "dd",
    "sync",
    "mknod",
    "mkfifo",
    "touch",
    "sha256sum",
    "printf",
    "test",
    "[",
];
pub const APPLETS: &[&str] = &[
    "init", "ctl", "sh", "echo", "cat", "ls", "mkdir", "cp", "mv", "rm", "ln", "readlink", "chmod",
    "pwd", "sleep", "true", "false", "uname", "mount", "umount", "insmod", "dmesg", "ps", "free",
    "kill", "poweroff", "reboot", "modprobe", "lsmod", "rmmod", "blkid", "boot", "kexec", "ip",
    "dhclient", "pxeboot", "wget",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub target: String,
    pub commands: Vec<String>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl Manifest {
    pub fn current(target: &str) -> Self {
        Self {
            version: 1,
            target: target.into(),
            commands: APPLETS.iter().map(|s| (*s).into()).collect(),
        }
    }
    pub fn with_tools(target: &str, coreutils: bool, brush: bool) -> Self {
        let mut value = Self::current(target);
        if coreutils {
            value
                .commands
                .extend(COREUTILS_EXTRA.iter().map(|s| (*s).into()));
        }
        if brush {
            if !coreutils {
                value
                    .commands
                    .extend(["printf".into(), "test".into(), "[".into()]);
            }
            value.commands.extend(["brush".into(), "minish".into()]);
        }
        value
    }
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > 64 * 1024 {
            return Err(invalid("command manifest exceeds 64KiB"));
        }
        let value: Self = serde_json::from_slice(bytes).map_err(io::Error::other)?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.version != 1 || self.target.len() > 128 || !self.target.contains("-linux-") {
            return Err(invalid("unsupported command manifest version/target"));
        }
        if self.commands.len() > 1024 || !self.commands.iter().any(|s| s == "init") {
            return Err(invalid(
                "command manifest must contain init and at most 1024 commands",
            ));
        }
        let mut seen = BTreeSet::new();
        for name in &self.commands {
            if name.is_empty()
                || name.len() > 64
                || (name != "["
                    && !name.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                    }))
                || name == "rbb"
                || !seen.insert(name)
            {
                return Err(invalid("unsafe or duplicate command name"));
            }
        }
        Ok(())
    }
    pub fn select(&self, preset: &str, explicit: &[String]) -> io::Result<Vec<String>> {
        self.validate()?;
        let base = [
            "init", "ctl", "sh", "echo", "cat", "ls", "mkdir", "pwd", "sleep", "true", "false",
            "uname", "mount", "umount", "dmesg", "ps", "free", "kill", "poweroff", "reboot",
        ];
        let extras: &[&str] = match preset {
            "minimal" => &[],
            "diskboot" => &[
                "cp", "mv", "rm", "ln", "readlink", "chmod", "insmod", "modprobe", "lsmod",
                "rmmod", "blkid", "boot", "kexec",
            ],
            "netboot" => &["boot", "kexec", "ip", "dhclient", "pxeboot", "wget"],
            "rescue" => &[],
            _ => return Err(invalid("unknown image preset")),
        };
        let mut selected = if !explicit.is_empty() {
            explicit.to_vec()
        } else if preset == "rescue" {
            self.commands.clone()
        } else {
            base.iter().chain(extras).map(|s| (*s).into()).collect()
        };
        if self.commands.iter().any(|n| n == "brush") && selected.iter().any(|n| n == "sh") {
            for name in ["printf", "test", "["] {
                if !selected.iter().any(|n| n == name) {
                    selected.push(name.into());
                }
            }
        }
        let mut seen = BTreeSet::new();
        for name in &selected {
            if !self.commands.contains(name) || !seen.insert(name) {
                return Err(invalid("selected command is unavailable or repeated"));
            }
        }
        if !seen.contains(&"init".to_string()) {
            return Err(invalid("command selection must include init"));
        }
        Ok(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_and_untrusted_inventories() {
        let mut m = Manifest::current("aarch64-unknown-linux-musl");
        assert!(!m.select("minimal", &[]).unwrap().contains(&"wget".into()));
        assert!(m.select("netboot", &[]).unwrap().contains(&"wget".into()));
        assert!(m
            .select("rescue", &["init".into(), "missing".into()])
            .is_err());
        assert!(m.select("minimal", &["sh".into()]).is_err());
        m.commands.push("../escape".into());
        assert!(m.validate().is_err());
        assert!(Manifest::parse(b"{\"version\":2}").is_err());
    }
}
