//! Shared data model for disk and network boot. No loading side effects here.
use crate::invalid;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::io;

fn current_arch() -> String {
    std::env::consts::ARCH.into()
}
fn version() -> u32 {
    1
}
fn timeout() -> u64 {
    5000
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub source: String,
    pub sha256: Option<String>,
}
impl Artifact {
    pub fn local(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            sha256: None,
        }
    }
    pub fn validate(&self, require_hash: bool) -> io::Result<()> {
        if self.source.is_empty() || self.source.len() > 4096 || self.source.contains('\0') {
            return Err(invalid("invalid boot artifact source"));
        }
        match self.sha256.as_deref() {
            Some(value) if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            None if !require_hash => {}
            _ => return Err(invalid("boot artifact needs a 64-digit SHA-256 digest")),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootEntry {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default = "current_arch")]
    pub architecture: String,
    pub kernel: Artifact,
    #[serde(default)]
    pub initrd: Vec<Artifact>,
    #[serde(default)]
    pub cmdline: String,
}
impl BootEntry {
    pub fn validate(&self, require_hashes: bool) -> io::Result<()> {
        if self.id.is_empty()
            || self.id.len() > 200
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(invalid(
                "boot entry id must be ASCII letters/digits/_.- (1..200 bytes)",
            ));
        }
        if !["x86_64", "aarch64"].contains(&self.architecture.as_str()) {
            return Err(invalid("unsupported boot architecture"));
        }
        if self.title.len() > 512
            || self.title.chars().any(char::is_control)
            || self.cmdline.len() > 4095
            || self.cmdline.contains(['\0', '\n', '\r'])
            || self.initrd.len() > 16
        {
            return Err(invalid(
                "boot title, command line or initrd count exceeds limits",
            ));
        }
        self.kernel.validate(require_hashes)?;
        for initrd in &self.initrd {
            initrd.validate(require_hashes)?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    #[serde(default = "version")]
    pub version: u32,
    pub entries: Vec<BootEntry>,
}
impl Manifest {
    pub fn parse(text: &str, require_hashes: bool) -> io::Result<Self> {
        if text.len() > 1024 * 1024 {
            return Err(invalid("boot manifest exceeds 1MiB"));
        }
        let manifest: Self = toml::from_str(text).map_err(io::Error::other)?;
        if manifest.version != 1 || manifest.entries.is_empty() || manifest.entries.len() > 256 {
            return Err(invalid(
                "unsupported/empty boot manifest or too many entries",
            ));
        }
        let mut ids = BTreeSet::new();
        for entry in &manifest.entries {
            entry.validate(require_hashes)?;
            if !ids.insert(&entry.id) {
                return Err(invalid("duplicate boot entry id"));
            }
        }
        Ok(manifest)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootConfig {
    /// "disk", a mounted directory, or a native manifest path/URL.
    pub source: String,
    pub entry: Option<String>,
    #[serde(default)]
    pub append: String,
    #[serde(default)]
    pub require_hashes: bool,
    #[serde(default)]
    pub menu: bool,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
}
impl BootConfig {
    pub fn validate(&self) -> io::Result<()> {
        if self.source.is_empty()
            || self.source.contains('\0')
            || self.append.contains(['\0', '\n', '\r'])
            || self.append.len() > 2048
            || self.timeout_ms > 300_000
        {
            return Err(invalid(
                "invalid boot source, command-line append, or menu timeout",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_manifest_and_hash_policy() {
        let source = "[[entries]]\nid='test'\nkernel={source='/kernel'}";
        assert!(Manifest::parse(source, false).is_ok());
        assert!(Manifest::parse(source, true).is_err());
        assert!(Manifest::parse(&format!("{source}\nunknown=1"), false).is_err());
        assert!(Manifest::parse(&format!("{source}\n{source}"), false).is_err());
    }
}
