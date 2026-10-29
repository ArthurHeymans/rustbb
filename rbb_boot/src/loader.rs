//! Bounded artifact staging, integrity checks, and file-based Linux kexec.
//! Loading is separate from execution so PID 1 can coordinate service shutdown.
use crate::{
    invalid,
    local::{self, Catalog},
    model::{Artifact, BootConfig, BootEntry, Manifest},
};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const MAX_KERNEL: u64 = 128 * 1024 * 1024;
pub const MAX_INITRD: u64 = 256 * 1024 * 1024;

pub fn copy_verified(
    mut input: impl Read,
    mut output: impl Write,
    max: u64,
    digest: Option<&str>,
) -> io::Result<u64> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or_else(|| invalid("artifact size overflow"))?;
        if total > max {
            return Err(invalid("boot artifact exceeds size limit"));
        }
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    if let Some(expected) = digest {
        let actual: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(invalid(format!(
                "SHA-256 mismatch: expected {expected}, got {actual}"
            )));
        }
    }
    Ok(total)
}

pub fn validate_kernel(mut file: &File, architecture: &str) -> io::Result<()> {
    if architecture != std::env::consts::ARCH {
        return Err(invalid(format!(
            "kernel architecture {architecture} does not match {}",
            std::env::consts::ARCH
        )));
    }
    let mut header = [0; 4096];
    let count = file.read(&mut header)?;
    match architecture {
        "aarch64" if count >= 64 && &header[56..60] == b"ARMd" => {
            let size = u64::from_le_bytes(header[16..24].try_into().unwrap());
            if size == 0 || size > 1024 * 1024 * 1024 || header[24] & 1 != 0 { return Err(invalid("unsupported arm64 Image size/endianness")); }
        }
        "x86_64" if count >= 0x238 && &header[0x202..0x206] == b"HdrS" && header[0x1fe..0x200] == [0x55, 0xaa] && header[0x236] & 1 != 0 => {},
        _ => return Err(invalid("expected uncompressed arm64 Image or x86_64 bzImage, matching the running architecture")),
    }
    Ok(())
}

fn stage_artifact(
    artifact: &Artifact,
    directory: &Path,
    max: u64,
    policy: &crate::fetch::Policy,
) -> io::Result<tempfile::NamedTempFile> {
    let input = crate::fetch::artifact(&artifact.source, policy)?;
    let mut output = tempfile::NamedTempFile::new_in(directory)?;
    copy_verified(input, &mut output, max, artifact.sha256.as_deref())?;
    output.as_file_mut().seek(SeekFrom::Start(0))?;
    Ok(output)
}

pub struct Staged {
    pub entry: BootEntry,
    kernel: tempfile::TempPath,
    initrd: Option<tempfile::TempPath>,
}
impl Staged {
    pub fn prepare(entry: &BootEntry, directory: &Path, require_hashes: bool) -> io::Result<Self> {
        Self::prepare_with_policy(
            entry,
            directory,
            require_hashes,
            &crate::fetch::Policy::default(),
        )
    }
    pub fn prepare_with_policy(
        entry: &BootEntry,
        directory: &Path,
        require_hashes: bool,
        policy: &crate::fetch::Policy,
    ) -> io::Result<Self> {
        entry.validate(require_hashes)?;
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let kernel = stage_artifact(&entry.kernel, directory, MAX_KERNEL, policy)?;
        validate_kernel(kernel.as_file(), &entry.architecture)?;
        let mut combined = if entry.initrd.is_empty() {
            None
        } else {
            Some(tempfile::NamedTempFile::new_in(directory)?)
        };
        let mut size = 0;
        if let Some(output) = &mut combined {
            for artifact in &entry.initrd {
                let input = crate::fetch::artifact(&artifact.source, policy)?;
                size += copy_verified(
                    input,
                    &mut *output,
                    MAX_INITRD - size,
                    artifact.sha256.as_deref(),
                )?;
                let padding = (4 - size % 4) % 4;
                output.write_all(&[0; 3][..padding as usize])?;
                size += padding;
                if size > MAX_INITRD {
                    return Err(invalid("combined initrd exceeds 256MiB"));
                }
            }
            output.flush()?;
        }
        Ok(Self {
            entry: entry.clone(),
            // Close all writable descriptors before kexec_file_load: the
            // kernel's deny_write_access otherwise rejects them with ETXTBSY.
            kernel: kernel.into_temp_path(),
            initrd: combined.map(tempfile::NamedTempFile::into_temp_path),
        })
    }
    pub fn load(&self) -> io::Result<()> {
        file_load(&self.kernel, self.initrd.as_ref().map(|f| f.as_ref()), &self.entry.cmdline)
            .map_err(|error| io::Error::other(format!("kexec_file_load: {error}; check CONFIG_KEXEC_FILE, capabilities and kernel signature policy")))?;
        eprintln!(
            "rbb-boot: staged {} ({}) for kexec",
            self.entry.id, self.entry.title
        );
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
fn file_load(kernel: &Path, initrd: Option<&Path>, cmdline: &str) -> io::Result<()> {
    let request = kexec_loader::KexecRequest {
        kernel: kernel.into(),
        initrd: initrd.map(PathBuf::from),
        cmdline: cmdline.into(),
    };
    kexec_loader::load_dry(&request).map_err(io::Error::other)
}

#[cfg(target_arch = "aarch64")]
fn file_load(kernel: &Path, initrd: Option<&Path>, cmdline: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // kexec-loader 0.17 references libc::SYS_kexec_file_load, which libc 0.2.191
    // does not expose for aarch64-musl. Linux's stable asm-generic ABI is 294.
    // This narrow adapter fills that tested portability gap, not a new loader.
    const SYS_KEXEC_FILE_LOAD: libc::c_long = 294;
    let kernel = File::open(kernel)?;
    let initrd = initrd.map(File::open).transpose()?;
    let cmdline = std::ffi::CString::new(cmdline).map_err(io::Error::other)?;
    let flags: libc::c_ulong = if initrd.is_some() { 0 } else { 4 }; // KEXEC_FILE_NO_INITRAMFS
    let fd = initrd.as_ref().map_or(-1, AsRawFd::as_raw_fd);
    // SAFETY: owned read-only fds, valid NUL-terminated command line, correct
    // byte length including NUL and flags from Linux's UAPI. No execution here.
    let result = unsafe {
        libc::syscall(
            SYS_KEXEC_FILE_LOAD,
            kernel.as_raw_fd(),
            fd,
            cmdline.as_bytes_with_nul().len() as libc::c_ulong,
            cmdline.as_ptr(),
            flags,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn kexec_loaded() -> bool {
    fs::read_to_string("/sys/kernel/kexec_loaded").is_ok_and(|s| s.trim() == "1")
}
pub fn execute() -> io::Result<()> {
    if unsafe { libc::getpid() } != 1 {
        return Err(invalid("kernel execution must be coordinated by PID 1"));
    }
    if !kexec_loaded() {
        return Err(invalid("no kexec kernel is loaded"));
    }
    nix::unistd::sync();
    nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_KEXEC).map_err(io::Error::from)?;
    unreachable!()
}

pub fn catalog(source: &str, require_hashes: bool) -> io::Result<Catalog> {
    let config = BootConfig {
        source: source.into(),
        entry: None,
        append: String::new(),
        require_hashes,
        menu: false,
        timeout_ms: 5000,
        tls_ca: None,
        manifest_key: None,
        signature: None,
    };
    catalog_config(&config, &crate::fetch::Policy::default())
}

pub fn catalog_config(config: &BootConfig, policy: &crate::fetch::Policy) -> io::Result<Catalog> {
    let source = if config.source == "dhcp" {
        crate::network::saved_boot_url()?
    } else {
        config.source.clone()
    };
    let require_hashes = config.require_hashes || config.manifest_key.is_some();
    if source.contains("://") {
        let url = url::Url::parse(&source).map_err(io::Error::other)?;
        return crate::netboot::catalog(&url, config, policy).map(Catalog::entries);
    }
    if source == "disk" {
        if config.manifest_key.is_some() {
            return Err(invalid(
                "manifest signatures require a native manifest, not raw disk config scanning",
            ));
        }
        return Catalog::disks();
    }
    let path = Path::new(&source);
    if path.is_dir() {
        if config.manifest_key.is_some() {
            return Err(invalid(
                "manifest signatures require a native manifest file",
            ));
        }
        return Catalog::directory(path);
    }
    let text = local::read_config(path)?;
    if let Some(key) = &config.manifest_key {
        let signature = config
            .signature
            .clone()
            .unwrap_or_else(|| format!("{source}.sig"));
        let mut bytes = Vec::new();
        crate::fetch::artifact(&signature, policy)?
            .take(1025)
            .read_to_end(&mut bytes)?;
        crate::verification::verify(text.as_bytes(), Path::new(key), &bytes)?;
    }
    let mut manifest = Manifest::parse(&text, require_hashes)?;
    let parent = fs::canonicalize(path)?
        .parent()
        .ok_or_else(|| invalid("manifest has no directory"))?
        .to_path_buf();
    for entry in &mut manifest.entries {
        for artifact in std::iter::once(&mut entry.kernel).chain(entry.initrd.iter_mut()) {
            if !Path::new(&artifact.source).is_absolute() && !artifact.source.contains("://") {
                artifact.source = parent
                    .join(&artifact.source)
                    .to_str()
                    .ok_or_else(|| invalid("manifest artifact paths must be UTF-8"))?
                    .into();
            }
        }
    }
    Ok(Catalog::entries(manifest.entries))
}

pub fn choose<'a>(entries: &'a [BootEntry], config: &BootConfig) -> io::Result<&'a BootEntry> {
    let supported: Vec<_> = entries
        .iter()
        .filter(|e| e.architecture == std::env::consts::ARCH)
        .collect();
    let default = if let Some(id) = &config.entry {
        *supported
            .iter()
            .find(|e| &e.id == id)
            .ok_or_else(|| invalid(format!("boot entry not found for this architecture: {id}")))?
    } else {
        *supported
            .first()
            .ok_or_else(|| invalid("no boot entries for this architecture"))?
    };
    if !config.menu {
        return Ok(default);
    }
    for (index, entry) in supported.iter().enumerate() {
        eprintln!("{}: {} [{}]", index + 1, entry.title, entry.id);
    }
    eprintln!(
        "rbb-boot: choose number or id (default {}, timeout {}ms):",
        default.id, config.timeout_ms
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(config.timeout_ms);
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(default);
        }
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut fd,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if ready == -1 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
        if ready == 0 {
            return Ok(default);
        }
        let mut byte = [0];
        if io::stdin().read(&mut byte)? == 0 || byte[0] == b'\n' {
            break;
        }
        bytes.push(byte[0]);
        if bytes.len() > 255 {
            return Err(invalid("boot menu input exceeds limit"));
        }
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(io::Error::other)?
        .trim();
    if text.is_empty() {
        return Ok(default);
    }
    if let Ok(number) = text.parse::<usize>() {
        if number > 0 {
            if let Some(entry) = supported.get(number - 1) {
                return Ok(entry);
            }
        }
    }
    supported
        .into_iter()
        .find(|e| e.id == text)
        .ok_or_else(|| invalid("invalid boot menu selection"))
}

pub fn load(config: &BootConfig) -> io::Result<()> {
    config.validate()?;
    let policy = crate::fetch::Policy {
        tls_ca: config.tls_ca.as_ref().map(PathBuf::from),
        ..crate::fetch::Policy::default()
    };
    let catalog = catalog_config(config, &policy)?;
    let mut entry = choose(&catalog.entries, config)?.clone();
    if !config.append.is_empty() {
        entry.cmdline.push(' ');
        entry.cmdline.push_str(&config.append);
    }
    let staged = Staged::prepare_with_policy(
        &entry,
        &PathBuf::from("/run/rbb/boot/stage"),
        config.require_hashes || config.manifest_key.is_some(),
        &policy,
    )?;
    staged.load()?;
    // Both source mounts and staging files can now be released; the kernel has
    // copied the loaded segments. They are never held across final execution.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hashes_and_bounds_fail_closed() {
        let mut output = Vec::new();
        let good = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(
            copy_verified(&b"abc"[..], &mut output, 3, Some(good)).unwrap(),
            3
        );
        assert!(copy_verified(&b"abcd"[..], Vec::new(), 3, Some(good)).is_err());
        assert!(copy_verified(&b"bad"[..], Vec::new(), 3, Some(good)).is_err());
        assert!(execute().is_err());
    }
    #[test]
    fn staged_kernel_has_no_open_writer() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("kernel");
        let mut header = vec![0u8; 4096];
        if std::env::consts::ARCH == "x86_64" {
            header[0x1fe..0x200].copy_from_slice(&[0x55, 0xaa]);
            header[0x202..0x206].copy_from_slice(b"HdrS");
            header[0x236] = 1;
        } else {
            header[56..60].copy_from_slice(b"ARMd");
            header[16..24].copy_from_slice(&4096u64.to_le_bytes());
        }
        fs::write(&source, header).unwrap();
        let entry = BootEntry {
            id: "test".into(),
            title: "test".into(),
            architecture: std::env::consts::ARCH.into(),
            kernel: Artifact::local(source.to_str().unwrap()),
            initrd: Vec::new(),
            cmdline: String::new(),
        };
        let staged = Staged::prepare(&entry, &directory.path().join("stage"), false).unwrap();
        for fd in fs::read_dir("/proc/self/fd").unwrap().flatten() {
            assert_ne!(
                fs::read_link(fd.path()).ok().as_deref(),
                Some(staged.kernel.as_ref())
            );
        }
    }
}
