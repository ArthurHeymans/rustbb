# Local Linux kernel boot

`rbb-boot` uses one boot-entry model for all supported sources. Local disk boot
scans filesystems read-only, stages kernel/initrd bytes, loads a second kernel,
and coordinates the actual jump through PID 1. No external kexec executable is
used. `kexec-loader` provides `kexec_file_load` on x86_64. Its 0.17 release does
not compile for aarch64-musl because libc lacks its syscall constant there, so
arm64 uses a narrow adapter for Linux's stable asm-generic syscall 294. Both
paths have real second-kernel boot tests. The running kernel must enable
`CONFIG_KEXEC_FILE`, and capability/signature/lockdown policy still applies.

## Configuration

```toml
[boot]
source = "disk"
entry = "vda1-linux"
menu = false
timeout_ms = 5000
append = "console=ttyS0"
```

`[boot]` and `[root]` are mutually exclusive. Boot runs before services start;
preparation failures record `/run/rbb/boot-error` and start configured rescue
services. Once loading succeeds, PID 1 syncs and executes kexec. Kernel parameters
`rbb.boot.source=`, `rbb.boot.entry=` and `rbb.boot.menu` override selection.

Sources can be `disk`, an already-mounted filesystem directory, or a native TOML
manifest file. Disk scan mounts are read-only, nosuid/nodev/noexec; ext4 uses
`noload` to avoid journal-replay writes. Mounts stay alive until artifacts are
staged/loaded, and are released before final handoff.

Supported disk configurations:

- BLS Type #1 under `/loader/entries` or `/boot/loader/entries`, parsed using the
  unmodified **MPL-2.0 boot-loader-spec 0.1.5 library**. Architecture filtering,
  multiple initrds, titles, options and explicit entry selection are supported.
- Declarative GRUB `menuentry`, `linux`/`linuxefi`, `initrd`/`initrdefi` blocks.
  This is not a GRUB interpreter. Procedural conditionals, unresolved variables,
  cross-device root selection and custom DTBs are rejected, not executed.
- Syslinux/extlinux LABEL, MENU LABEL, LINUX/KERNEL, INITRD and APPEND. INCLUDE,
  CONFIG and custom DTBs are not supported yet.

Artifact/config paths are confined to the scanned filesystem. Parent traversal,
variables, GRUB device prefixes and symlink escapes are rejected. Entry IDs are
prefixed with the source device, e.g. `vda1-linux` for `linux.conf` on `/dev/vda1`,
`vda1-grub-0` or an explicit GRUB `--id`, and `vda1-extlinux-LABEL`. The default is
the first supported entry in deterministic discovery order, not a claim to
implement every distribution's version-ranking or saved-entry policy. Use an
explicit `entry` for unattended boot. Menu input/default selection is bounded.

## Native manifest and integrity

```toml
version = 1
[[entries]]
id = "linux"
title = "Linux"
architecture = "aarch64" # x86_64 or aarch64; defaults to running architecture
cmdline = "console=ttyAMA0 rdinit=/init"
kernel = { source = "/boot/Image", sha256 = "64_HEX_DIGITS" }
initrd = [
  { source = "/boot/early.cpio", sha256 = "64_HEX_DIGITS" },
  { source = "/boot/initramfs.cpio.gz", sha256 = "64_HEX_DIGITS" },
]
```

Point `[boot].source` to this file; set `require_hashes = true` to require digests
for **every** artifact. Relative paths are relative to the manifest directory.
Digests are always checked when supplied, even if `require_hashes` is false.
The manifest must itself be trusted: an untrusted manifest can replace both the
image and its hash. Signed-manifest/network trust policy is a subsequent layer.

Copying is streamed and bounded (128MiB kernel, 256MiB combined initrd), with
SHA-256 verification before loading. Initrds are concatenated in order with
four-byte padding. The kernel format must match the running architecture:
x86_64 bzImage or uncompressed little-endian arm64 Image. File-based arm64 kexec
uses the running kernel's device-tree handoff; custom DTBs and classic segment-
based kexec are not implemented by this path.

All writable staging file descriptors are closed before the kernel loader runs.
This matters: Linux denies write access to loaded kernel/initrd files and returns
`ETXTBSY` if the staging writer is still open. Staging files and scan mounts are
RAII-managed and never retained across execution.

## Commands and live service shutdown

```sh
boot --source disk --no-load          # list entries, no load or execution
boot --source disk --entry vda1-linux --no-exec
kexec -l /boot/Image --initrd /boot/initramfs.cpio.gz --append 'console=ttyAMA0'
kexec -e                              # ask rbb PID 1 to shut down and jump
```

`boot` also accepts `--menu`, `--timeout-ms`, `--append`, and `--require-hashes`.
Without `--no-exec`, it requests execution via the rbb control socket after
loading. `ctl kexec` requires a loaded image and real PID 1 mode; standalone host
supervision refuses it. PID 1 stops restarts, sends TERM, escalates to KILL at the
deadline, reaps children and syncs before executing kexec. A staged image does
not automatically stop services or execute itself.

MPL source availability and notices are included in images at
`/etc/rbb/third-party-notices.txt`; our separate source files remain MIT OR
Apache-2.0. Do not remove notices or relicense the dependency as our own code.

## End-to-end verification

```sh
python3 scripts/test-kexec.py --arch x86_64
python3 scripts/test-kexec.py --arch arm64
```

Use the dedicated kernels from `scripts/build-test-kernels.py` or `--kernel`.
The test disk contains BLS, basic GRUB and extlinux entries, kernels and multiple
initrd fragments. Each case must actually reach a service in the **second**
kernel with a distinct command-line stage marker. A native manifest case checks
all artifact digests. The live-supervisor case starts a TERM-resistant service,
loads without executing, requests `kexec -e`, requires KILL escalation and
shutdown completion, and verifies the new kernel's boot ID differs. A tampered
hash must reach rescue without loading or executing a kernel. Artifacts and
serial logs are under `target/kexec-tests/ARCH/`.
