//! Guarded initramfs -> disk-root transition. Prepare can fail safely; after
//! exec() begins moving mounts, failure is fatal and PID 1 must stay alive.
use crate::{
    cmdline::Cmdline,
    device::{self, Selector},
    invalid,
};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use serde::Deserialize;
use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn fstype() -> String {
    "ext4".into()
}
fn init() -> Vec<String> {
    vec!["/sbin/init".into()]
}
fn wait_ms() -> u64 {
    10_000
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    pub device: String,
    #[serde(default = "fstype")]
    pub fstype: String,
    #[serde(default)]
    pub options: String,
    #[serde(default)]
    pub writable: bool,
    #[serde(default = "wait_ms")]
    pub wait_ms: u64,
    #[serde(default = "init")]
    pub init: Vec<String>,
}
impl RootConfig {
    pub fn validate(&self) -> io::Result<()> {
        Selector::parse(&self.device)?;
        if self.fstype.is_empty()
            || self.fstype.contains(['\0', '/', ' '])
            || self.options.contains('\0')
            || self.init.is_empty()
            || !self.init[0].starts_with('/')
            || self.init.iter().any(|s| s.contains('\0'))
            || self.wait_ms > 300_000
        {
            return Err(invalid("invalid root filesystem configuration"));
        }
        Ok(())
    }
    pub fn from_cmdline(cmdline: &Cmdline, configured: Option<Self>) -> io::Result<Option<Self>> {
        let mut root = match configured {
            Some(root) => root,
            None => match cmdline.get("root") {
                Some(device) => Self {
                    device: device.into(),
                    fstype: fstype(),
                    options: String::new(),
                    writable: false,
                    wait_ms: wait_ms(),
                    init: init(),
                },
                None => return Ok(None),
            },
        };
        if let Some(device) = cmdline.get("root") {
            root.device = device.into();
        }
        if let Some(kind) = cmdline.get("rootfstype") {
            root.fstype = kind.into();
        }
        if let Some(options) = cmdline.get("rootflags") {
            root.options = options.into();
        }
        if let Some(value) = cmdline.get("rootwait").filter(|v| !v.is_empty()) {
            root.wait_ms = value
                .parse::<u64>()
                .map_err(io::Error::other)?
                .checked_mul(1000)
                .ok_or_else(|| invalid("rootwait overflow"))?;
        }
        // Unlike traditional unbounded rootwait, this init always has a deadline.
        if let Some(value) = cmdline.get("rbb.rootwait_ms") {
            root.wait_ms = value.parse().map_err(io::Error::other)?;
        }
        if cmdline.contains("rw") {
            root.writable = true;
        }
        if cmdline.contains("ro") {
            root.writable = false;
        }
        if let Some(path) = cmdline.get("init") {
            root.init[0] = path.into();
        }
        root.init.extend(cmdline.init_args.clone());
        root.validate()?;
        Ok(Some(root))
    }
}

pub fn mount_options(options: &str, writable: bool) -> (MsFlags, String) {
    let mut flags = if writable {
        MsFlags::empty()
    } else {
        MsFlags::MS_RDONLY
    };
    let mut data = Vec::new();
    for option in options.split(',').filter(|s| !s.is_empty()) {
        match option {
            "ro" => flags.insert(MsFlags::MS_RDONLY),
            "rw" => flags.remove(MsFlags::MS_RDONLY),
            "nosuid" => flags.insert(MsFlags::MS_NOSUID),
            "nodev" => flags.insert(MsFlags::MS_NODEV),
            "noexec" => flags.insert(MsFlags::MS_NOEXEC),
            "noatime" => flags.insert(MsFlags::MS_NOATIME),
            "defaults" => {}
            _ => data.push(option),
        }
    }
    (flags, data.join(","))
}

fn require_initramfs() -> io::Result<()> {
    if unsafe { libc::getpid() } != 1 {
        return Err(invalid("switch_root is restricted to PID 1"));
    }
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: constant NUL-terminated path and valid output storage.
    if unsafe { libc::statfs(c"/".as_ptr(), &mut stat) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if stat.f_type as u64 != 0x858458f6 && stat.f_type as u64 != 0x01021994 {
        return Err(invalid(
            "old / is not ramfs or tmpfs; refusing destructive root handoff",
        ));
    }
    if !Path::new("/etc/rbb/initramfs-release").is_file() {
        return Err(invalid("missing rbb initramfs identity marker"));
    }
    Ok(())
}

fn open_init(root: &File, path: &str) -> io::Result<File> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let path = CString::new(path).map_err(io::Error::other)?;
    // IN_ROOT anchors absolute symlinks inside the new root; NO_MAGICLINKS
    // excludes /proc-style links. Pin the executable before touching old root.
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve: 0x10 | 0x02,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of_val(&how),
        )
    };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd as i32) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
        return Err(invalid("real init must be an executable regular file"));
    }
    Ok(file)
}

pub struct PreparedRoot {
    path: PathBuf,
    root: File,
    executable: File,
    args: Vec<CString>,
    mounted: bool,
}
impl Drop for PreparedRoot {
    fn drop(&mut self) {
        if self.mounted {
            let _ = umount2(&self.path, MntFlags::MNT_DETACH);
        }
    }
}
impl PreparedRoot {
    pub fn prepare(config: &RootConfig) -> io::Result<Self> {
        require_initramfs()?;
        config.validate()?;
        let device = device::wait_for(
            &Selector::parse(&config.device)?,
            Duration::from_millis(config.wait_ms),
        )?;
        let path = PathBuf::from("/newroot");
        fs::create_dir_all(&path)?;
        let (flags, data) = mount_options(&config.options, config.writable);
        mount(
            Some(device.as_path()),
            &path,
            Some(config.fstype.as_str()),
            flags,
            Some(data.as_str()),
        )
        .map_err(io::Error::from)?;
        let prepare = (|| {
            let root = File::open(&path)?;
            if root.metadata()?.dev() == fs::metadata("/")?.dev() {
                return Err(invalid("new root must be on a different filesystem"));
            }
            let executable = open_init(&root, &config.init[0])?;
            for name in ["dev", "proc", "sys", "run"] {
                let metadata = fs::symlink_metadata(path.join(name))?;
                if !metadata.is_dir() {
                    return Err(invalid(format!(
                        "real root /{name} must be a directory, not a symlink"
                    )));
                }
            }
            let args = config
                .init
                .iter()
                .map(|a| CString::new(a.as_str()).map_err(io::Error::other))
                .collect::<io::Result<_>>()?;
            Ok(Self {
                path: path.clone(),
                root,
                executable,
                args,
                mounted: true,
            })
        })();
        if prepare.is_err() {
            let _ = umount2(&path, MntFlags::MNT_DETACH);
        }
        prepare
    }

    /// Irreversible phase. On error, callers must keep PID 1 alive, not attempt
    /// to restart services using the old filesystem/configuration.
    pub fn exec(mut self) -> io::Result<()> {
        require_initramfs()?;
        let old = File::open("/")?;
        let old_id = mount_id(&old)?;
        // Validate the mounted root fd still refers to the expected mount.
        if self.root.metadata()?.dev() != fs::metadata(&self.path)?.dev() {
            return Err(invalid("new root changed during preparation"));
        }
        mount::<str, str, str, str>(None, "/", None, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None)
            .map_err(io::Error::from)?;
        self.mounted = false;
        for name in ["dev", "proc", "sys", "run"] {
            let source = format!("/{name}");
            mount(
                Some(source.as_str()),
                &self.path.join(name),
                None::<&str>,
                MsFlags::MS_MOVE,
                None::<&str>,
            )
            .map_err(io::Error::from)?;
        }
        std::env::set_current_dir(&self.path)?;
        mount(
            Some(self.path.as_path()),
            Path::new("/"),
            None::<&str>,
            MsFlags::MS_MOVE,
            None::<&str>,
        )
        .map_err(io::Error::from)?;
        nix::unistd::chroot(".").map_err(io::Error::from)?;
        std::env::set_current_dir("/")?;
        if let Err(error) = clean_old_root(&old, old_id) {
            eprintln!("rbb-init: old-root cleanup: {error}");
        }
        drop(old);
        // Permit interpreter scripts as init: the pinned descriptor must survive
        // exec of the interpreter. Only this file fd is intentionally inherited.
        nix::fcntl::fcntl(
            &self.executable,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
        )
        .map_err(io::Error::from)?;
        let env: Vec<CString> = std::env::vars_os()
            .map(|(key, value)| {
                let mut bytes = key.as_os_str().as_bytes().to_vec();
                bytes.push(b'=');
                bytes.extend_from_slice(value.as_os_str().as_bytes());
                CString::new(bytes).map_err(io::Error::other)
            })
            .collect::<io::Result<_>>()?;
        eprintln!(
            "rbb-init: switching to real init {:?} as PID 1",
            self.args[0]
        );
        nix::unistd::fexecve(&self.executable, &self.args, &env).map_err(io::Error::from)?;
        unreachable!()
    }
}

fn mount_id(file: &File) -> io::Result<u64> {
    let mut stat: libc::statx = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_MNT_ID,
            &mut stat,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    if stat.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(io::Error::other("kernel did not return mount identity"));
    }
    Ok(stat.stx_mnt_id)
}

fn clean_old_root(directory: &File, root_mount: u64) -> io::Result<()> {
    for entry in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
        let name = entry?.file_name();
        let child = nix::fcntl::openat(
            directory,
            name.as_os_str(),
            nix::fcntl::OFlag::O_DIRECTORY
                | nix::fcntl::OFlag::O_NOFOLLOW
                | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        );
        match child {
            Ok(fd) => {
                let child = File::from(fd);
                // statx mount IDs also exclude bind mounts on the same device.
                if mount_id(&child)? != root_mount {
                    continue;
                }
                clean_old_root(&child, root_mount)?;
                let _ = nix::unistd::unlinkat(
                    directory,
                    name.as_os_str(),
                    nix::unistd::UnlinkatFlags::RemoveDir,
                );
            }
            Err(nix::errno::Errno::ENOTDIR | nix::errno::Errno::ELOOP) => {
                let _ = nix::unistd::unlinkat(
                    directory,
                    name.as_os_str(),
                    nix::unistd::UnlinkatFlags::NoRemoveDir,
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_overrides_are_bounded() {
        let args =
            Cmdline::parse("root=UUID=abc rootfstype=vfat rw rootwait=2 init=/bin/init -- -v")
                .unwrap();
        let root = RootConfig::from_cmdline(&args, None).unwrap().unwrap();
        assert_eq!(root.wait_ms, 2000);
        assert!(root.writable);
        assert_eq!(root.init, ["/bin/init", "-v"]);
        assert!(RootConfig::from_cmdline(
            &Cmdline::parse("root=/dev/vda rootwait=99999999999").unwrap(),
            None
        )
        .is_err());
        assert!(require_initramfs().is_err());
    }
    #[test]
    fn absolute_init_symlinks_are_scoped_to_new_root() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sbin")).unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        fs::write(dir.path().join("bin/real-init"), "test").unwrap();
        fs::set_permissions(
            dir.path().join("bin/real-init"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("/bin/real-init", dir.path().join("sbin/init")).unwrap();
        let root = File::open(dir.path()).unwrap();
        assert!(open_init(&root, "/sbin/init").is_ok());
        assert!(open_init(&root, "/../../etc/passwd").is_err());
    }
}
