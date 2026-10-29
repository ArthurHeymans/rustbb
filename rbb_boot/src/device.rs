//! Read-only block-device discovery. Kernel drivers still perform filesystem I/O.
use crate::invalid;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selector {
    Path(PathBuf),
    Uuid(String),
    PartUuid(String),
    Label(String),
}
impl Selector {
    pub fn parse(text: &str) -> io::Result<Self> {
        if text.contains('\0') {
            return Err(invalid("NUL in root selector"));
        }
        for (prefix, kind) in [("UUID=", 0), ("PARTUUID=", 1), ("LABEL=", 2)] {
            if let Some(value) = text.strip_prefix(prefix) {
                if value.is_empty() || value.contains('/') {
                    return Err(invalid("empty or invalid device identity"));
                }
                return Ok(match kind {
                    0 => Self::Uuid(value.to_lowercase()),
                    1 => Self::PartUuid(value.to_lowercase()),
                    _ => Self::Label(value.into()),
                });
            }
        }
        if text.starts_with("/dev/")
            && !Path::new(text)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Ok(Self::Path(text.into()));
        }
        Err(invalid(
            "root must be /dev/PATH, UUID=, PARTUUID=, or LABEL=",
        ))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Device {
    pub path: PathBuf,
    pub fstype: Option<String>,
    pub uuid: Option<String>,
    pub label: Option<String>,
    pub partuuid: Option<String>,
}

pub fn probe(path: &Path) -> io::Result<Device> {
    let mut device = Device {
        path: path.into(),
        ..Device::default()
    };
    if let Ok(fs) = ext4_view::Ext4::load_from_path(path) {
        device.fstype = Some("ext4".into());
        device.uuid = Some(fs.uuid().to_string().to_lowercase());
        device.label = fs.label().to_str().ok().map(str::to_string);
        return Ok(device);
    }
    let file = File::open(path)?;
    if let Ok(fs) = fatfs::FileSystem::new(file, fatfs::FsOptions::new()) {
        device.fstype = Some("vfat".into());
        let id = fs.volume_id();
        device.uuid = Some(format!("{:04x}-{:04x}", id >> 16, id & 0xffff));
        device.label = Some(fs.volume_label().trim().to_string());
    }
    Ok(device)
}

fn guid(bytes: &[u8; 16]) -> String {
    // GPT GUID fields are mixed-endian, unlike a raw UUID byte string.
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{}",
        u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
        u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
        bytes[8],
        bytes[9],
        bytes[10..]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

fn partition_ids(disk: &Path, sector_size: u64) -> BTreeMap<u64, String> {
    let mut ids = BTreeMap::new();
    let Ok(mut file) = File::open(disk) else {
        return ids;
    };
    if let Ok(table) = gptman::GPT::read_from(&mut file, sector_size) {
        for (_, entry) in table.iter().filter(|(_, e)| e.is_used()) {
            // Linux sysfs `start` is always in 512-byte sectors.
            if let Some(start) = entry.starting_lba.checked_mul(sector_size / 512) {
                ids.insert(start, guid(&entry.unique_partition_guid));
            }
        }
        return ids;
    }
    let _ = file.seek(SeekFrom::Start(0));
    if let Ok(table) = mbrman::MBR::read_from(&mut file, 512) {
        // Primary MBR entries only. Extended/logical partitions need explicit paths.
        for (number, entry) in table.iter().filter(|(_, e)| e.is_used()) {
            ids.insert(
                entry.starting_lba as u64,
                format!(
                    "{:08x}-{number:02x}",
                    u32::from_le_bytes(table.header.disk_signature)
                ),
            );
        }
    }
    ids
}

pub fn discover_in(sysfs: &Path, dev: &Path) -> io::Result<Vec<Device>> {
    let mut entries: Vec<_> = fs::read_dir(sysfs)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    let mut partitions = BTreeMap::new();
    for entry in &entries {
        let path = entry.path();
        if path.join("partition").exists() {
            continue;
        }
        let sector_size = fs::read_to_string(path.join("queue/logical_block_size"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(512);
        for (start, id) in partition_ids(&dev.join(entry.file_name()), sector_size) {
            partitions.insert((entry.file_name(), start), id);
        }
    }
    let mut result = Vec::new();
    for entry in entries {
        let path = entry.path();
        let device_path = dev.join(entry.file_name());
        let mut device = match probe(&device_path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if path.join("partition").exists() {
            if let (Ok(real), Ok(start)) = (
                fs::canonicalize(&path),
                fs::read_to_string(path.join("start")),
            ) {
                if let (Some(parent), Ok(start)) = (
                    real.parent().and_then(Path::file_name),
                    start.trim().parse::<u64>(),
                ) {
                    device.partuuid = partitions.get(&(parent.to_os_string(), start)).cloned();
                }
            }
        }
        result.push(device);
    }
    Ok(result)
}

pub fn discover() -> io::Result<Vec<Device>> {
    discover_in(Path::new("/sys/class/block"), Path::new("/dev"))
}

pub fn select(selector: &Selector, devices: &[Device]) -> io::Result<Option<PathBuf>> {
    let matched: Vec<_> = devices
        .iter()
        .filter(|d| match selector {
            Selector::Path(path) => &d.path == path,
            Selector::Uuid(id) => d
                .uuid
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(id)),
            Selector::PartUuid(id) => d
                .partuuid
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(id)),
            Selector::Label(label) => d.label.as_ref() == Some(label),
        })
        .collect();
    if matched.len() > 1 {
        return Err(invalid(
            "ambiguous device selector; multiple matching disks",
        ));
    }
    Ok(matched.first().map(|d| d.path.clone()))
}

pub fn wait_for(selector: &Selector, timeout: Duration) -> io::Result<PathBuf> {
    let start = Instant::now();
    loop {
        if let Selector::Path(path) = selector {
            use std::os::unix::fs::FileTypeExt;
            if fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device()) {
                return Ok(path.clone());
            }
        } else if let Some(path) = select(selector, &discover()?)? {
            return Ok(path);
        }
        if start.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("root device not found: {selector:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn kernel_release() -> io::Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/osrelease")?
        .trim()
        .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selectors_are_unambiguous_and_not_relative() {
        assert!(Selector::parse("/dev/../etc/passwd").is_err());
        assert!(Selector::parse("UUID=").is_err());
        assert!(Selector::parse("vda").is_err());
        let d = Device {
            path: "/dev/vda".into(),
            uuid: Some("ABCD".into()),
            ..Device::default()
        };
        let s = Selector::parse("UUID=abcd").unwrap();
        assert_eq!(
            select(&s, std::slice::from_ref(&d)).unwrap(),
            Some("/dev/vda".into())
        );
        assert!(select(&s, &[d.clone(), d]).is_err());
    }
    #[test]
    fn guid_has_gpt_byte_order() {
        let bytes = [
            0x78, 0x56, 0x34, 0x12, 0x34, 0x12, 0x78, 0x56, 0x90, 0xab, 0xcd, 0xef, 0x01, 0x23,
            0x45, 0x67,
        ];
        assert_eq!(guid(&bytes), "12345678-1234-5678-90ab-cdef01234567");
    }
}
