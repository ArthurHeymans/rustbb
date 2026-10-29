//! Boot-entry discovery from read-only filesystems. GRUB/extlinux support is
//! intentionally declarative, not a shell interpreter or cross-disk resolver.
use crate::{
    device, invalid,
    model::{Artifact, BootEntry},
};
use boot_loader_spec::{BLSEntry, BLSValue};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

pub fn read_config(path: &Path) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let mut text = String::new();
    file.take(1024 * 1024 + 1).read_to_string(&mut text)?;
    if text.len() > 1024 * 1024 {
        return Err(invalid("boot config exceeds 1MiB"));
    }
    Ok(text)
}

fn read_under(root: &Path, path: &Path) -> io::Result<String> {
    let path = fs::canonicalize(path)?;
    if !path.starts_with(fs::canonicalize(root)?) {
        return Err(invalid("config symlink escapes boot filesystem"));
    }
    read_config(&path)
}

fn rooted(root: &Path, source: &str) -> io::Result<Artifact> {
    if source.contains(['$', '\0'])
        || source.starts_with('(')
        || Path::new(source)
            .components()
            .any(|p| matches!(p, Component::ParentDir))
    {
        return Err(invalid(
            "unsupported variable, device reference or traversal in boot path",
        ));
    }
    let root = fs::canonicalize(root)?;
    let path = fs::canonicalize(root.join(source.trim_start_matches('/')))?;
    if !path.starts_with(&root) || !path.is_file() {
        return Err(invalid(
            "boot artifact escapes source filesystem or is not a file",
        ));
    }
    Ok(Artifact::local(
        path.to_str()
            .ok_or_else(|| invalid("boot paths must be UTF-8"))?,
    ))
}
fn value(v: &BLSValue) -> &str {
    match v {
        BLSValue::Value(v) | BLSValue::ValueWithComment(v, _) => v,
    }
}

pub fn parse_bls(text: &str, root: &Path, id: &str) -> io::Result<BootEntry> {
    let bls = BLSEntry::parse(text).map_err(invalid)?;
    if bls.devicetree.is_some() || bls.devicetree_overlay.is_some() || value(&bls.linux).is_empty()
    {
        return Err(invalid(
            "custom DTB/EFI-only BLS entries need another loader",
        ));
    }
    let architecture = match bls.architecture.as_ref().map(value) {
        Some("x64" | "x86_64") => "x86_64",
        Some("aa64" | "aarch64") => "aarch64",
        None => std::env::consts::ARCH,
        _ => return Err(invalid("unsupported BLS architecture")),
    };
    let cmdline = bls.options.iter().map(value).collect::<Vec<_>>().join(" ");
    if cmdline.contains('$') {
        return Err(invalid("unresolved BLS command-line variable"));
    }
    let entry = BootEntry {
        id: id.into(),
        title: bls.title.as_ref().map(value).unwrap_or(id).into(),
        architecture: architecture.into(),
        kernel: rooted(root, value(&bls.linux))?,
        initrd: bls
            .initrd
            .iter()
            .map(|v| rooted(root, value(v)))
            .collect::<io::Result<_>>()?,
        cmdline,
    };
    entry.validate(false)?;
    Ok(entry)
}

#[derive(Default)]
struct Draft {
    id: String,
    title: String,
    kernel: Option<String>,
    initrd: Vec<String>,
    cmdline: String,
    unsupported: bool,
}
impl Draft {
    fn finish(self, root: &Path) -> io::Result<Option<BootEntry>> {
        if self.unsupported {
            return Err(invalid(format!(
                "{}: unsupported procedural config, variable or custom device tree",
                self.id
            )));
        }
        let Some(kernel) = self.kernel else {
            return Ok(None);
        };
        if self.cmdline.contains('$') {
            return Err(invalid("unresolved command-line variable"));
        }
        let entry = BootEntry {
            id: self.id,
            title: self.title,
            architecture: std::env::consts::ARCH.into(),
            kernel: rooted(root, &kernel)?,
            initrd: self
                .initrd
                .iter()
                .map(|p| rooted(root, p))
                .collect::<io::Result<_>>()?,
            cmdline: self.cmdline,
        };
        entry.validate(false)?;
        Ok(Some(entry))
    }
}

pub fn parse_extlinux(text: &str, root: &Path) -> io::Result<Vec<BootEntry>> {
    let mut entries = Vec::new();
    let mut current = None::<Draft>;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        if word.eq_ignore_ascii_case("label") {
            if let Some(draft) = current.take() {
                if let Some(entry) = draft.finish(root)? {
                    entries.push(entry);
                }
            }
            current = Some(Draft {
                id: format!("extlinux-{rest}"),
                title: rest.into(),
                ..Draft::default()
            });
        } else if let Some(draft) = &mut current {
            match word.to_ascii_lowercase().as_str() {
                "linux" | "kernel" => draft.kernel = Some(rest.into()),
                "initrd" => draft.initrd = rest.split(',').map(|s| s.trim().to_string()).collect(),
                "append" => draft.cmdline = rest.into(),
                "menu" => {
                    if let Some(title) = rest
                        .strip_prefix("LABEL ")
                        .or_else(|| rest.strip_prefix("label "))
                    {
                        draft.title = title.into();
                    }
                }
                "fdtdir" | "fdt" | "devicetree" | "include" | "config" => draft.unsupported = true,
                _ => {}
            }
        } else if word.eq_ignore_ascii_case("include") || word.eq_ignore_ascii_case("config") {
            return Err(invalid("extlinux INCLUDE/CONFIG not supported yet"));
        }
    }
    if let Some(draft) = current {
        if let Some(entry) = draft.finish(root)? {
            entries.push(entry);
        }
    }
    Ok(entries)
}

pub fn parse_grub(text: &str, root: &Path) -> io::Result<Vec<BootEntry>> {
    let mut entries = Vec::new();
    let mut current = None::<Draft>;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let words = shlex::split(line).ok_or_else(|| invalid("invalid GRUB quoting"))?;
        let Some(word) = words.first().map(String::as_str) else {
            continue;
        };
        if word == "menuentry" {
            if current.is_some() {
                return Err(invalid("nested GRUB menuentry"));
            }
            let title = words
                .get(1)
                .ok_or_else(|| invalid("GRUB menuentry needs title"))?
                .clone();
            let id = words
                .windows(2)
                .find_map(|w| (w[0] == "--id").then(|| w[1].clone()))
                .unwrap_or_else(|| format!("grub-{}", entries.len()));
            current = Some(Draft {
                id,
                title,
                ..Draft::default()
            });
        } else if word == "}" {
            if let Some(draft) = current.take() {
                if let Some(entry) = draft.finish(root)? {
                    entries.push(entry);
                }
            }
        } else if current.is_none()
            && matches!(
                word,
                "search" | "source" | "configfile" | "submenu" | "if" | "for" | "while"
            )
        {
            return Err(invalid("unsupported procedural GRUB configuration"));
        } else if current.is_none()
            && word == "set"
            && words.get(1).is_some_and(|v| v.starts_with("root="))
        {
            return Err(invalid("cross-device GRUB root selection is not supported"));
        } else if let Some(draft) = &mut current {
            match word {
                "linux" | "linuxefi" | "linux16" if words.len() >= 2 => {
                    if draft.kernel.is_some() {
                        draft.unsupported = true;
                    }
                    draft.kernel = Some(words[1].clone());
                    draft.cmdline = words[2..]
                        .iter()
                        .map(|w| {
                            if w.contains(char::is_whitespace) {
                                format!("\"{w}\"")
                            } else {
                                w.clone()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                }
                "initrd" | "initrdefi" => draft.initrd.extend_from_slice(&words[1..]),
                "echo" | "insmod" => {}
                _ => draft.unsupported = true,
            }
        }
    }
    if current.is_some() {
        return Err(invalid("unterminated GRUB menuentry"));
    }
    Ok(entries)
}

pub fn scan(root: &Path) -> io::Result<Vec<BootEntry>> {
    let mut entries = Vec::new();
    for directory in ["loader/entries", "boot/loader/entries"] {
        if let Ok(files) = fs::read_dir(root.join(directory)) {
            let mut files = files.collect::<Result<Vec<_>, _>>()?;
            files.sort_by_key(|e| e.file_name());
            for file in files
                .into_iter()
                .take(256)
                .filter(|f| f.path().extension().is_some_and(|e| e == "conf"))
            {
                let id = file
                    .path()
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                match read_under(root, &file.path()).and_then(|text| parse_bls(&text, root, &id)) {
                    Ok(entry) => entries.push(entry),
                    Err(error) => eprintln!("rbb-boot: skip {}: {error}", file.path().display()),
                }
            }
        }
    }
    for (name, grub) in [
        ("grub/grub.cfg", true),
        ("boot/grub/grub.cfg", true),
        ("extlinux/extlinux.conf", false),
        ("boot/extlinux/extlinux.conf", false),
        ("syslinux/syslinux.cfg", false),
        ("boot/syslinux/syslinux.cfg", false),
    ] {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        match read_under(root, &path).and_then(|text| {
            if grub {
                parse_grub(&text, root)
            } else {
                parse_extlinux(&text, root)
            }
        }) {
            Ok(parsed) => entries.extend(parsed),
            Err(error) => eprintln!("rbb-boot: skip {}: {error}", path.display()),
        }
    }
    Ok(entries)
}

struct Mount {
    directory: tempfile::TempDir,
}
impl Drop for Mount {
    fn drop(&mut self) {
        let _ = umount2(self.directory.path(), MntFlags::MNT_DETACH);
    }
}
/// Keeps scan mounts alive until all artifacts have been staged/loaded.
pub struct Catalog {
    pub entries: Vec<BootEntry>,
    _mounts: Vec<Mount>,
}
impl Catalog {
    pub fn entries(entries: Vec<BootEntry>) -> Self {
        Self {
            entries,
            _mounts: Vec::new(),
        }
    }
    pub fn disks() -> io::Result<Self> {
        let directory = PathBuf::from("/run/rbb/boot/mounts");
        fs::create_dir_all(&directory)?;
        let mut result = Self {
            entries: Vec::new(),
            _mounts: Vec::new(),
        };
        for device in device::discover()? {
            let Some(kind) = device.fstype.as_deref() else {
                continue;
            };
            let candidate = tempfile::tempdir_in(&directory)?;
            let options = if kind == "ext4" { "noload" } else { "" };
            if let Err(error) = mount(
                Some(device.path.as_path()),
                candidate.path(),
                Some(kind),
                MsFlags::MS_RDONLY | MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
                Some(options),
            ) {
                eprintln!("rbb-boot: cannot scan {}: {error}", device.path.display());
                continue;
            }
            let mounted = Mount {
                directory: candidate,
            };
            let prefix = device
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            for mut entry in scan(mounted.directory.path())? {
                entry.id = format!("{prefix}-{}", entry.id);
                result.entries.push(entry);
            }
            result._mounts.push(mounted);
        }
        if result.entries.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no supported boot entries on disk",
            ));
        }
        Ok(result)
    }
    pub fn directory(root: &Path) -> io::Result<Self> {
        Ok(Self {
            entries: scan(root)?,
            _mounts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("kernel"), "kernel").unwrap();
        fs::write(root.path().join("initrd"), "initrd").unwrap();
        root
    }
    #[test]
    fn bls_uses_reusable_parser_and_confines_paths() {
        let root = fixture();
        let e = parse_bls(
            "title Test\nlinux /kernel\ninitrd /initrd\noptions quiet",
            root.path(),
            "test",
        )
        .unwrap();
        assert_eq!(e.initrd.len(), 1);
        assert_eq!(e.cmdline, "quiet");
        assert!(parse_bls("linux /../etc/passwd", root.path(), "test").is_err());
        assert!(parse_bls("linux /kernel\noptions $kernelopts", root.path(), "test").is_err());
        std::os::unix::fs::symlink("/etc/passwd", root.path().join("escape")).unwrap();
        assert!(parse_bls("linux /escape", root.path(), "test").is_err());
    }
    #[test]
    fn declarative_grub_and_extlinux() {
        let root = fixture();
        let grub = "menuentry 'Test OS' --id test {\n linux /kernel quiet\n initrd /initrd\n}\n";
        assert_eq!(parse_grub(grub, root.path()).unwrap()[0].title, "Test OS");
        assert!(parse_grub(
            "menuentry 'bad' {\nset root=other\nlinux /kernel\n}\n",
            root.path()
        )
        .is_err());
        let ext =
            "LABEL test\n MENU LABEL Test OS\n LINUX /kernel\n INITRD /initrd\n APPEND quiet\n";
        assert_eq!(
            parse_extlinux(ext, root.path()).unwrap()[0].id,
            "extlinux-test"
        );
    }
}
