mod archive;
mod inventory;
pub use archive::{Conflict, MAX_ARCHIVE_BYTES};
pub use inventory::command_manifest;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};

#[derive(Clone, Debug)]
pub struct Entry {
    pub mode: u32,
    pub data: Vec<u8>,
    pub device: (u32, u32),
    pub uid: u32,
    pub gid: u32,
}
impl Entry {
    pub fn file(data: Vec<u8>, mode: u32) -> Self {
        Self {
            mode: 0o100000 | (mode & 0o777),
            data,
            device: (0, 0),
            uid: 0,
            gid: 0,
        }
    }
    pub fn directory(mode: u32) -> Self {
        Self {
            mode: 0o040000 | (mode & 0o7777),
            data: Vec::new(),
            device: (0, 0),
            uid: 0,
            gid: 0,
        }
    }
    pub fn link(target: &str) -> Self {
        Self {
            mode: 0o120777,
            data: target.as_bytes().to_vec(),
            device: (0, 0),
            uid: 0,
            gid: 0,
        }
    }
}

#[derive(Clone, Default)]
pub struct Image {
    pub entries: BTreeMap<String, Entry>,
    // Implicit parents must not overwrite an existing layer's directory metadata.
    pub(crate) implicit_dirs: BTreeSet<String>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

pub fn archive_path(path: &str) -> io::Result<String> {
    let relative = path.trim_start_matches('/');
    if relative.is_empty()
        || relative.as_bytes().contains(&0)
        || relative
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
        || Path::new(relative)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(invalid(format!("unsafe archive path: {path:?}")));
    }
    Ok(relative.into())
}

impl Image {
    pub fn insert(&mut self, path: &str, entry: Entry) -> io::Result<()> {
        let path = archive_path(path)?;
        if path == "TRAILER!!!" {
            return Err(invalid("reserved cpio trailer path"));
        }
        let mut parent = Path::new(&path).parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
            let name = p.to_str().ok_or_else(|| invalid("non-UTF8 path"))?;
            if let Some(existing) = self.entries.get(name) {
                if existing.mode & 0o170000 != 0o040000 {
                    return Err(invalid(format!("parent {name} is not a directory")));
                }
            } else {
                self.entries.insert(name.into(), Entry::directory(0o755));
                self.implicit_dirs.insert(name.into());
            }
            parent = p.parent();
        }
        if let Some(existing) = self.entries.get(&path) {
            if existing.mode & 0o170000 == 0o040000 && entry.mode & 0o170000 == 0o040000 {
                if self.implicit_dirs.remove(&path) {
                    self.entries.insert(path, entry);
                }
                return Ok(());
            }
            return Err(invalid(format!("duplicate archive path: {path}")));
        }
        // A symlink/file must not replace a directory that already has descendants.
        if entry.mode & 0o170000 != 0o040000
            && self
                .entries
                .keys()
                .any(|k| k.starts_with(&format!("{path}/")))
        {
            return Err(invalid(format!("{path} already contains entries")));
        }
        self.entries.insert(path, entry);
        Ok(())
    }

    pub fn add_host(&mut self, source: &Path, destination: &str) -> io::Result<()> {
        let metadata = fs::symlink_metadata(source)?;
        let kind = metadata.file_type();
        if kind.is_dir() {
            self.insert(
                destination,
                Entry::directory(metadata.permissions().mode() & 0o777),
            )?;
            let mut children: Vec<_> = fs::read_dir(source)?.collect::<Result<_, _>>()?;
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let name = child.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| invalid("overlay names must be UTF-8"))?;
                self.add_host(&child.path(), &format!("{destination}/{name}"))?;
            }
        } else if kind.is_symlink() {
            let target = fs::read_link(source)?;
            self.insert(
                destination,
                Entry {
                    mode: 0o120777,
                    data: target.as_os_str().as_bytes().to_vec(),
                    device: (0, 0),
                    uid: 0,
                    gid: 0,
                },
            )?;
        } else if kind.is_file() {
            self.insert(
                destination,
                Entry::file(fs::read(source)?, metadata.permissions().mode()),
            )?;
        } else {
            return Err(invalid(format!(
                "unsupported overlay file: {}",
                source.display()
            )));
        }
        Ok(())
    }

    pub fn write_newc(&self, mut output: impl Write) -> io::Result<()> {
        let mut bytes = 0;
        for (i, (name, entry)) in self.entries.iter().enumerate() {
            bytes += record(
                &mut output,
                name,
                entry,
                u32::try_from(i + 1).map_err(io::Error::other)?,
            )?;
        }
        bytes += record(&mut output, "TRAILER!!!", &Entry::file(Vec::new(), 0), 0)?;
        let padding = (512 - bytes % 512) % 512;
        output.write_all(&vec![0; padding])?;
        Ok(())
    }
}

fn record(output: &mut impl Write, name: &str, entry: &Entry, inode: u32) -> io::Result<usize> {
    let size = u32::try_from(entry.data.len()).map_err(io::Error::other)?;
    let name_size = u32::try_from(name.len() + 1).map_err(io::Error::other)?;
    // ino, mode, uid, gid, nlink, mtime, filesize, devmajor/minor, rdevmajor/minor, namesize, check.
    write!(output, "070701{inode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{size:08x}{:08x}{:08x}{:08x}{:08x}{name_size:08x}{:08x}",
        entry.mode, entry.uid, entry.gid, if entry.mode & 0o170000 == 0o040000 { 2 } else { 1 }, 0, 0, 0, entry.device.0, entry.device.1, 0)?;
    output.write_all(name.as_bytes())?;
    output.write_all(&[0])?;
    let name_pad = (4 - (110 + name_size as usize) % 4) % 4;
    output.write_all(&[0; 3][..name_pad])?;
    output.write_all(&entry.data)?;
    let data_pad = (4 - entry.data.len() % 4) % 4;
    output.write_all(&[0; 3][..data_pad])?;
    Ok(110 + name_size as usize + name_pad + entry.data.len() + data_pad)
}

/// Reject dynamic executables: initramfs deliberately has no ELF interpreter.
/// ELF64 little-endian is supported (x86_64, aarch64, riscv64, ...).
pub fn validate_static_elf(data: &[u8]) -> io::Result<()> {
    if data.len() < 64 || &data[..6] != b"\x7fELF\x02\x01" {
        return Err(invalid("expected a little-endian ELF64 executable"));
    }
    let u16_at = |n| u16::from_le_bytes(data[n..n + 2].try_into().unwrap());
    if ![2, 3].contains(&u16_at(16)) {
        return Err(invalid("ELF is not executable"));
    }
    let offset = usize::try_from(u64::from_le_bytes(data[32..40].try_into().unwrap()))
        .map_err(io::Error::other)?;
    let width = u16_at(54) as usize;
    let count = u16_at(56) as usize;
    if width < 56
        || count == 0
        || offset
            .checked_add(width * count)
            .is_none_or(|end| end > data.len())
    {
        return Err(invalid("invalid ELF program headers"));
    }
    for i in 0..count {
        let start = offset + i * width;
        if u32::from_le_bytes(data[start..start + 4].try_into().unwrap()) == 3 {
            return Err(invalid(
                "binary has PT_INTERP; build a static musl executable",
            ));
        }
    }
    Ok(())
}

pub fn base_image(binary: Vec<u8>) -> io::Result<Image> {
    preset_image(binary, "rescue", &[])
}

pub fn preset_image(binary: Vec<u8>, preset: &str, commands: &[String]) -> io::Result<Image> {
    let mut manifest = command_manifest(&binary)?;
    manifest.commands = manifest.select(preset, commands)?;
    let mut image = Image::default();
    for path in [
        "bbin", "bin", "sbin", "etc/rbb", "dev", "proc", "sys", "run", "tmp", "root",
    ] {
        image.insert(
            path,
            Entry::directory(if path == "tmp" { 0o1777 } else { 0o755 }),
        )?;
    }
    image.insert("bbin/rbb", Entry::file(binary, 0o755))?;
    image.insert(
        "etc/rbb/initramfs-release",
        Entry::file(b"rbb-initramfs v1\n".to_vec(), 0o644),
    )?;
    image.insert(
        "etc/rbb/commands.json",
        Entry::file(
            serde_json::to_vec(&manifest).map_err(io::Error::other)?,
            0o644,
        ),
    )?;
    for applet in &manifest.commands {
        image.insert(&format!("bbin/{applet}"), Entry::link("rbb"))?;
        image.insert(
            &format!("bin/{applet}"),
            Entry::link(&format!("../bbin/{applet}")),
        )?;
    }
    image.insert("init", Entry::link("bbin/init"))?;
    image.insert(
        "etc/rbb/third-party-notices.txt",
        Entry::file(
            include_bytes!("../../docs/third-party-notices.txt").to_vec(),
            0o644,
        ),
    )?;
    image.insert(
        "dev/console",
        Entry {
            mode: 0o020600,
            data: Vec::new(),
            device: (5, 1),
            uid: 0,
            gid: 0,
        },
    )?;
    image.insert(
        "dev/null",
        Entry {
            mode: 0o020666,
            data: Vec::new(),
            device: (1, 3),
            uid: 0,
            gid: 0,
        },
    )?;
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_traversal_and_symlink_parents() {
        for path in ["", "/", "a/../b", "./x", "a//b", "a/", "a\0b"] {
            assert!(archive_path(path).is_err());
        }
        let mut image = Image::default();
        image.insert("etc", Entry::link("/tmp")).unwrap();
        assert!(image
            .insert("etc/file", Entry::file(vec![], 0o644))
            .is_err());
        assert!(image.insert("etc", Entry::file(vec![], 0o644)).is_err());
    }
    #[test]
    fn deterministic_headers_and_padding() {
        let mut image = Image::default();
        image
            .insert("a/file", Entry::file(b"hello".to_vec(), 0o644))
            .unwrap();
        let mut one = Vec::new();
        let mut two = Vec::new();
        image.write_newc(&mut one).unwrap();
        image.write_newc(&mut two).unwrap();
        assert_eq!(one, two);
        assert_eq!(one.len() % 512, 0);
        assert_eq!(&one[..6], b"070701");
        assert_eq!(&one[22..38], b"0000000000000000"); // root uid/gid
        assert_eq!(&one[46..54], b"00000000"); // zero mtime
    }
    #[test]
    fn static_elf_validation() {
        assert!(validate_static_elf(b"not elf").is_err());
        let mut elf = vec![0; 120];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[16] = 2;
        elf[32] = 64;
        elf[54] = 56;
        elf[56] = 1;
        assert!(validate_static_elf(&elf).is_ok());
        elf[64] = 3;
        assert!(validate_static_elf(&elf).is_err());
        elf[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(validate_static_elf(&elf).is_err());
    }
}
