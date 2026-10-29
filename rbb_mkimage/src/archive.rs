//! Bounded newc/CRC and gzip import. Never extract archive paths onto the host.
use crate::{archive_path, invalid, Entry, Image};
use clap::ValueEnum;
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{self, Cursor, Read};
use std::path::Path;

pub const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;
const MAX_FILE: usize = 128 * 1024 * 1024;
const MAX_HEADERS: usize = 65536;

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum Conflict {
    #[default]
    Error,
    Keep,
    Replace,
}

type LinkKey = (usize, u32, u32, u32);
struct Raw {
    path: String,
    entry: Entry,
    link: Option<LinkKey>,
}

fn field(header: &[u8], offset: usize) -> io::Result<usize> {
    usize::from_str_radix(
        std::str::from_utf8(&header[offset..offset + 8]).map_err(io::Error::other)?,
        16,
    )
    .map_err(io::Error::other)
}

fn newc<'a>(
    mut input: &'a [u8],
    records: &mut Vec<Raw>,
    headers: &mut usize,
    segment: usize,
) -> io::Result<&'a [u8]> {
    loop {
        *headers += 1;
        if *headers > MAX_HEADERS {
            return Err(invalid("too many CPIO headers"));
        }
        if input.len() < 110 {
            return Err(invalid("truncated CPIO header or missing trailer"));
        }
        let header = &input[..110];
        // cpio 0.4 allocates names directly from namesize: validate before calling it.
        let namesize = field(header, 94)?;
        let size = field(header, 54)?;
        if !(1..=4097).contains(&namesize) || size > MAX_FILE {
            return Err(invalid("CPIO name/file exceeds limit"));
        }
        input = &input[110..];
        let mut reader = cpio::newc::Reader::new(Cursor::new(header).chain(&mut input))?;
        let meta = reader.entry().clone();
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;
        let _ = reader.finish()?;
        if data.len() != size {
            return Err(invalid("truncated CPIO data"));
        }
        if let Some(checksum) = meta.checksum() {
            if data
                .iter()
                .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
                != checksum
            {
                return Err(invalid("CPIO CRC checksum mismatch"));
            }
        }
        if meta.is_trailer() {
            if size != 0 {
                return Err(invalid("CPIO trailer has data"));
            }
            return Ok(input);
        }
        let mut name = meta.name();
        while let Some(rest) = name.strip_prefix("./") {
            name = rest;
        }
        let kind = meta.mode() & 0o170000;
        if [".", "/"].contains(&name) && kind == 0o040000 {
            continue;
        }
        let path = archive_path(name)?;
        if !matches!(
            kind,
            0o010000 | 0o020000 | 0o040000 | 0o060000 | 0o100000 | 0o120000
        ) || meta.nlink() == 0
            || meta.nlink() as usize > MAX_HEADERS
        {
            return Err(invalid("unsupported CPIO type/link count"));
        }
        if !matches!(kind, 0o100000 | 0o120000) && size != 0 {
            return Err(invalid("non-file CPIO entry has payload"));
        }
        if kind == 0o120000 && (data.is_empty() || data.contains(&0)) {
            return Err(invalid("empty/NUL symlink target"));
        }
        records.push(Raw {
            path,
            entry: Entry {
                mode: meta.mode() & 0o177777,
                data,
                device: (meta.rdev_major(), meta.rdev_minor()),
                uid: meta.uid(),
                gid: meta.gid(),
            },
            link: (kind == 0o100000 && meta.nlink() > 1).then_some((
                segment,
                meta.dev_major(),
                meta.dev_minor(),
                meta.ino(),
            )),
        });
    }
}

fn zeros(input: &[u8]) -> &[u8] {
    let count = input.iter().position(|b| *b != 0).unwrap_or(input.len());
    &input[count..]
}

impl Image {
    pub fn read_base(path: &Path, conflict: Conflict) -> io::Result<Self> {
        let file = File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(invalid("base must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_ARCHIVE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(invalid("base archive exceeds 256MiB"));
        }
        Self::read_archive(&bytes, conflict)
    }

    pub fn read_archive(bytes: &[u8], conflict: Conflict) -> io::Result<Self> {
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(invalid("base archive exceeds 256MiB"));
        }
        let mut input = bytes;
        let mut records = Vec::new();
        let (mut headers, mut segment, mut total) = (0, 0, 0usize);
        while !zeros(input).is_empty() {
            input = zeros(input);
            if input.starts_with(&[0x1f, 0x8b]) {
                let mut decoder = flate2::bufread::GzDecoder::new(input);
                let mut decoded = Vec::new();
                (&mut decoder)
                    .take((MAX_ARCHIVE_BYTES - total) as u64 + 1)
                    .read_to_end(&mut decoded)?;
                total = total
                    .checked_add(decoded.len())
                    .ok_or_else(|| invalid("archive size overflow"))?;
                if total > MAX_ARCHIVE_BYTES {
                    return Err(invalid("decompressed archive exceeds 256MiB"));
                }
                input = decoder.into_inner();
                let mut chunk = zeros(&decoded);
                while !chunk.is_empty() {
                    chunk = zeros(newc(chunk, &mut records, &mut headers, segment)?);
                    segment += 1;
                }
            } else {
                let before = input.len();
                input = newc(input, &mut records, &mut headers, segment)?;
                segment += 1;
                total = total
                    .checked_add(before - input.len())
                    .ok_or_else(|| invalid("archive size overflow"))?;
                if total > MAX_ARCHIVE_BYTES {
                    return Err(invalid("expanded archive exceeds 256MiB"));
                }
            }
        }
        if headers == 0 {
            return Err(invalid("empty base archive"));
        }
        let mut groups: BTreeMap<LinkKey, &Entry> = BTreeMap::new();
        for record in &records {
            if let Some(key) = record.link {
                if let Some(previous) = groups.get(&key) {
                    if (previous.mode, previous.uid, previous.gid)
                        != (record.entry.mode, record.entry.uid, record.entry.gid)
                        || (!previous.data.is_empty()
                            && !record.entry.data.is_empty()
                            && previous.data != record.entry.data)
                    {
                        return Err(invalid("inconsistent CPIO hardlink group"));
                    }
                }
                if !record.entry.data.is_empty() || !groups.contains_key(&key) {
                    groups.insert(key, &record.entry);
                }
            }
        }
        // Resolve hardlinks before merging. Independent regular files avoid unsafe
        // aliasing across archives and have deterministic inode/link metadata.
        let mut expanded = Vec::new();
        let mut size = 0usize;
        for record in &records {
            let source = record.link.map(|k| groups[&k]).unwrap_or(&record.entry);
            size = size
                .checked_add(source.data.len())
                .ok_or_else(|| invalid("hardlink size overflow"))?;
            if size > MAX_ARCHIVE_BYTES {
                return Err(invalid("materialized hardlinks exceed 256MiB"));
            }
            let mut entry = record.entry.clone();
            entry.data = source.data.clone();
            expanded.push((record.path.clone(), entry));
        }
        expanded.sort_by_key(|(name, _)| name.split('/').count());
        let mut image = Self::default();
        for (name, entry) in expanded {
            image.insert_with(&name, entry, conflict)?;
        }
        Ok(image)
    }

    pub fn insert_with(&mut self, path: &str, entry: Entry, conflict: Conflict) -> io::Result<()> {
        let path = archive_path(path)?;
        if let Some(old) = self.entries.get(&path) {
            let both_dirs = old.mode & 0o170000 == 0o040000 && entry.mode & 0o170000 == 0o040000;
            if both_dirs && self.implicit_dirs.remove(&path) {
                self.entries.insert(path, entry);
                return Ok(());
            }
            match conflict {
                Conflict::Keep => return Ok(()),
                Conflict::Error if !both_dirs => {
                    return Err(invalid(format!("duplicate archive path: {path}")))
                }
                Conflict::Error => return Ok(()),
                Conflict::Replace => {}
            }
            if entry.mode & 0o170000 != 0o040000
                && self
                    .entries
                    .keys()
                    .any(|k| k.starts_with(&format!("{path}/")))
            {
                return Err(invalid("cannot replace a nonempty directory"));
            }
            let old = self.entries.remove(&path).unwrap();
            if let Err(error) = self.insert(&path, entry) {
                self.entries.insert(path, old);
                return Err(error);
            }
            return Ok(());
        }
        self.insert(&path, entry)
    }

    pub fn merge(&mut self, other: Self, conflict: Conflict) -> io::Result<()> {
        let total = self
            .entries
            .values()
            .chain(other.entries.values())
            .try_fold(0usize, |n, e| n.checked_add(e.data.len()))
            .ok_or_else(|| invalid("merged image size overflow"))?;
        if total > MAX_ARCHIVE_BYTES || self.entries.len() + other.entries.len() > MAX_HEADERS {
            return Err(invalid("merged image exceeds size/entry limits"));
        }
        let mut next = self.clone();
        for (name, entry) in other.entries {
            let implicit = other.implicit_dirs.contains(&name);
            if implicit && next.entries.contains_key(&name) {
                continue;
            }
            next.insert_with(&name, entry, conflict)?;
            if implicit {
                next.implicit_dirs.insert(name);
            }
        }
        *self = next;
        Ok(())
    }

    pub fn preserve_init(&mut self) -> io::Result<()> {
        if let Some(entry) = self.entries.get("init").cloned() {
            if entry.mode & 0o170000 == 0o040000 {
                return Err(invalid("base init is a directory"));
            }
            self.insert("inito", entry)?;
            self.entries.remove("init");
        }
        Ok(())
    }

    /// Resolve only inside this in-memory image; never follow host symlinks.
    pub fn resolve(&self, path: &str) -> io::Result<&Entry> {
        let mut pending: VecDeque<String> =
            archive_path(path)?.split('/').map(String::from).collect();
        let mut resolved = Vec::new();
        let mut links = 0;
        while let Some(part) = pending.pop_front() {
            match part.as_str() {
                "" | "." => continue,
                ".." => {
                    if resolved.pop().is_none() {
                        return Err(invalid("symlink escapes image root"));
                    }
                    continue;
                }
                _ => resolved.push(part),
            }
            let name = resolved.join("/");
            let entry = self
                .entries
                .get(&name)
                .ok_or_else(|| invalid(format!("missing image path: {name}")))?;
            if entry.mode & 0o170000 == 0o120000 {
                links += 1;
                if links > 40 {
                    return Err(invalid("image symlink loop"));
                }
                resolved.pop();
                let target = std::str::from_utf8(&entry.data).map_err(io::Error::other)?;
                if target.starts_with('/') {
                    resolved.clear();
                }
                for component in target.split('/').rev() {
                    pending.push_front(component.into());
                }
            } else if !pending.is_empty() && entry.mode & 0o170000 != 0o040000 {
                return Err(invalid("image path parent is not a directory"));
            }
        }
        self.entries
            .get(&resolved.join("/"))
            .ok_or_else(|| invalid("image root is not executable"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn bytes(image: &Image) -> Vec<u8> {
        let mut out = Vec::new();
        image.write_newc(&mut out).unwrap();
        out
    }
    #[test]
    fn round_trip_gzip_concatenation_ownership_and_devices() {
        let mut image = Image::default();
        let mut e = Entry::file(b"base".to_vec(), 0o755);
        e.uid = 123;
        e.gid = 456;
        image.insert("file", e).unwrap();
        image
            .insert(
                "fifo",
                Entry {
                    mode: 0o010600,
                    data: vec![],
                    device: (0, 0),
                    uid: 1,
                    gid: 2,
                },
            )
            .unwrap();
        let raw = bytes(&image);
        let mut gz = flate2::GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).unwrap();
        let gz = gz.finish().unwrap();
        for input in [raw.clone(), gz.clone(), [raw, gz].concat()] {
            let parsed = Image::read_archive(&input, Conflict::Keep).unwrap();
            assert_eq!(parsed.entries["file"].uid, 123);
            assert_eq!(parsed.entries["file"].gid, 456);
            assert_eq!(parsed.entries["fifo"].mode, 0o010600);
            assert_eq!(bytes(&parsed), bytes(&image));
        }
    }
    #[test]
    fn crc_and_hardlink_payload_are_checked() {
        let mut raw = Vec::new();
        cpio::newc::Builder::new("one")
            .mode(0o100644)
            .ino(7)
            .nlink(2)
            .write(&mut raw, 0)
            .finish()
            .unwrap();
        let mut writer = cpio::newc::Builder::new("two")
            .mode(0o100644)
            .ino(7)
            .nlink(2)
            .write_crc(&mut raw, 3, 294);
        writer.write_all(b"abc").unwrap();
        writer.finish().unwrap();
        cpio::newc::Builder::new("TRAILER!!!")
            .write(&mut raw, 0)
            .finish()
            .unwrap();
        let image = Image::read_archive(&raw, Conflict::Error).unwrap();
        assert_eq!(image.entries["one"].data, b"abc");
        assert_eq!(image.entries["two"].data, b"abc");
        let offset = raw.windows(3).position(|w| w == b"abc").unwrap();
        raw[offset] ^= 1;
        assert!(Image::read_archive(&raw, Conflict::Error)
            .err()
            .unwrap()
            .to_string()
            .contains("checksum"));
    }
    #[test]
    fn hostile_headers_and_traversal_are_rejected() {
        let mut image = Image::default();
        image.insert("safe", Entry::file(vec![], 0o644)).unwrap();
        let raw = bytes(&image);
        for offset in [54, 94] {
            let mut bad = raw.clone();
            bad[offset..offset + 8].copy_from_slice(b"ffffffff");
            assert!(Image::read_archive(&bad, Conflict::Error).is_err());
        }
        let mut bad = raw.clone();
        bad[110..114].copy_from_slice(b"../x");
        assert!(Image::read_archive(&bad, Conflict::Error).is_err());
        assert!(Image::read_archive(&raw[..115], Conflict::Error).is_err());
        assert!(Image::read_archive(&[raw, b"junk".to_vec()].concat(), Conflict::Error).is_err());
    }
    #[test]
    fn merge_policy_is_atomic_and_cannot_replace_directory_with_link() {
        let mut first = Image::default();
        first
            .insert("dir/file", Entry::file(b"old".to_vec(), 0o644))
            .unwrap();
        let mut second = Image::default();
        second
            .insert("dir/file", Entry::file(b"new".to_vec(), 0o644))
            .unwrap();
        assert!(first.merge(second.clone(), Conflict::Error).is_err());
        assert_eq!(first.entries["dir/file"].data, b"old");
        first.merge(second.clone(), Conflict::Keep).unwrap();
        assert_eq!(first.entries["dir/file"].data, b"old");
        first.merge(second, Conflict::Replace).unwrap();
        assert_eq!(first.entries["dir/file"].data, b"new");
        assert!(first
            .insert_with("dir", Entry::link("/elsewhere"), Conflict::Replace)
            .is_err());
    }
    #[test]
    fn implicit_parents_do_not_relax_permissions() {
        let mut base = Image::default();
        base.insert("private", Entry::directory(0o700)).unwrap();
        let mut extra = Image::default();
        extra
            .insert("private/file", Entry::file(vec![], 0o600))
            .unwrap();
        base.merge(extra, Conflict::Replace).unwrap();
        assert_eq!(base.entries["private"].mode, 0o040700);
        let mut explicit = Image::default();
        explicit.insert("private", Entry::directory(0o750)).unwrap();
        base.merge(explicit, Conflict::Replace).unwrap();
        assert_eq!(base.entries["private"].mode, 0o040750);
    }
    #[test]
    fn preserved_init_and_bounded_virtual_resolution() {
        let mut image = Image::default();
        image.insert("init", Entry::link("bin/original")).unwrap();
        image
            .insert("bin/original", Entry::file(vec![], 0o755))
            .unwrap();
        image.preserve_init().unwrap();
        assert_eq!(image.resolve("inito").unwrap().mode, 0o100755);
        assert!(!image.entries.contains_key("init"));
        image
            .insert("escape", Entry::link("../../outside"))
            .unwrap();
        assert!(image.resolve("escape").is_err());
        image.insert("loop", Entry::link("loop")).unwrap();
        assert!(image.resolve("loop").is_err());
    }
}
