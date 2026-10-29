# Disk-root boot

`rbb-init` can mount a real disk root and replace itself with that root's init,
remaining PID 1. This is distinct from kexec, which starts a second kernel.

```toml
[root]
device = "UUID=e26e38ac-8aae-40c2-a52a-99400b5a338e"
fstype = "ext4"
init = ["/sbin/init"]
wait_ms = 10000
writable = false
options = ""
```

The root may instead be `/dev/vda`, `LABEL=name`, or `PARTUUID=...`. UUID and label
probing currently cover ext2/3/4 and FAT; GPT GUID and primary MBR partition IDs
are supported. Logical MBR partition identifiers are not yet resolved. Duplicate
matching identities fail rather than silently choosing a disk.

Kernel parameters override configured root values: `root=`, `rootfstype=`,
`rootflags=`, `ro`, `rw`, `rootwait=SECONDS`, `rbb.rootwait_ms=MILLISECONDS`,
`rootdelay=SECONDS`, and `init=`. Arguments following `--` are passed to real init.
Bare `rootwait` keeps the configured/default deadline; it never waits forever.
Wait/delay limits are at most five minutes. With no `[root]`, a kernel `root=`
parameter activates root boot with defaults (`ext4`, `/sbin/init`, read-only).

The kernel must have the disk and filesystem drivers built in, or they must be
loaded beforehand. Top-level `modules = ["virtio_blk", "ext4"]` and kernel
`rbb.modules=virtio_blk,ext4` load named modules before root discovery. Module
metadata is read from `/lib/modules/$(uname -r)/modules.dep` and `modules.alias`.
`modprobe`, `lsmod`, `rmmod`, and `blkid` are available alongside `insmod`.
Uncompressed, gzip and XZ module files are supported. Soft dependencies, automatic
uevent-driven modalias scanning, and zstd-compressed modules are not yet covered.

## Safety and failure behavior

- Root switching is restricted to PID 1 on ramfs/tmpfs, with the image builder's
  `/etc/rbb/initramfs-release` identity marker. Standalone host supervision never
  mounts or attempts root handoff.
- A new root must be a different filesystem. Its `dev`, `proc`, `sys`, and `run`
  directories must already exist and must not be symlinks. The new init must be
  a regular executable file. `openat2(RESOLVE_IN_ROOT|RESOLVE_NO_MAGICLINKS)`
  resolves and pins init, including absolute symlinks, inside the new root.
- Until preparation succeeds, errors unmount the candidate root, record
  `/run/rbb/boot-error`, and continue into configured rescue services.
- Once mount movement begins, errors are fatal: PID 1 stays alive and logs the
  error rather than attempting to use a half-switched filesystem.
- Special mounts are moved, `/` is replaced and chrooted, and old initramfs files
  are removed using descriptor-relative operations. Symlinks are not followed;
  statx mount identities exclude other mounts, including same-device bind mounts.
- The real init is executed in place. No supervisor remains behind it. This path
  runs before any configured services start; stopping live services for operator-
  initiated handoff is separate functionality.

## End-to-end tests

The test scripts only write disposable regular disk-image files under `target/`.
They do not require root, loop mounts, TAP interfaces or host block-device access.
They require `mkfs.ext4`, `sfdisk`, Rust musl targets, and QEMU system emulators.

Build dedicated kernels from an explicit Linux Git revision without changing the
source checkout. The build script exports a source archive and uses out-of-tree
builds, with configurations in `tests/kernels/`:

```sh
python3 scripts/build-test-kernels.py --source ../linux \
  --revision 6b4b543fa18757297ac756a6fe6e64f506b7844a
python3 scripts/test-disk-root.py --arch x86_64
python3 scripts/test-disk-root.py --arch arm64
```

Kernel build dependencies: make, Clang/LLVM/lld, bison, flex, bc, pkg-config and
libelf development headers. On Nix, pass **unwrapped target** Clang and lld via
`--cc /path/to/clang --ld /path/to/ld.lld`, but keep a host compiler capable of
finding the host C library (`--hostcc`). Compiler wrappers can inject host include
flags or ELF sections that are invalid for kernel/real-mode code. The build script
checks required configuration symbols and records kernel/config hashes.

Alternatively pass a suitable `--kernel PATH`. The dedicated kernels have ext4,
virtio block/net, console and kexec support built in. Each root test asserts that
the real disk init is PID 1, verifies a disk-only marker and moved filesystems,
checks read-only/read-write policy, and powers off. Cases exercise explicit paths,
UUID, LABEL, GPT PARTUUID, writable root, missing device and missing init. The
negative cases must reach rescue without starting an irreversible root switch.
