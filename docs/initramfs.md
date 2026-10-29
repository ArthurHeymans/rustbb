# Rust initramfs toolkit

This workspace has five companion crates for `rustbb`:

- **rbb-init**: Linux PID 1 and foreground service supervisor; also usable as an
  unprivileged host supervisor (`--supervise`).
- **rbb-cmds**: a statically linkable multicall rescue userland, binary `rbb`.
- **rbb-mkimage**: deterministic `newc` CPIO image builder, optionally gzip-compressed.
- **rbb-manifest**: shared, versioned target command inventory for feature-aware packaging.
- **rbb-boot**: device/module discovery, [disk-root handoff](disk-boot.md), and
  [local Linux kernel boot](kernel-boot.md), and [network boot](network-boot.md).

This is a working **MVP**, not s6 compatibility or full u-root parity. The generic
`rustbb` AST-based multicall builder remains independent; these boot-critical
commands use explicit library dispatch instead of rewriting PID 1 source code.
No Go, BusyBox, host shell, dynamic linker, external `cpio`, or root privileges are
needed to build an image. A Linux kernel is supplied separately.

## Build and boot

Requirements: stable Rust, an installed musl target, a target C/assembly compiler
for ring's TLS backend, and a matching Linux kernel and QEMU system emulator. x86_64 and arm64 are tested with TCG (no KVM/root
needed). Python 3 is needed for the tests.

From the workspace root:

```sh
rustup target add x86_64-unknown-linux-musl
cargo run -p rbb-mkimage -- --build -o target/rescue.cpio.gz
qemu-system-x86_64 -m 256M -kernel /path/to/bzImage \
  -initrd target/rescue.cpio.gz -append 'console=ttyS0 rdinit=/init' \
  -nographic -no-reboot -nic none
```

The default image boots a console command interpreter. Try `ctl status`, `ls /`,
`ps`, `mount`, `free`, then `poweroff`. QEMU's `Ctrl-a x` terminates the VM.
This rescue console is **unauthenticated**, deliberately. Do not expose it to
untrusted users. The small build's commands are intentionally limited. The default
`--build --preset rescue` enables optional [uutils and brush](rescue-userland.md);
`--build --preset minimal` excludes those dependencies. A prebuilt binary's
features cannot be changed by selecting different aliases.

On NixOS, if the host C linker injects its own loader, use Rust's bundled linker:

```sh
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld"
```

The QEMU test harness selects this automatically. Guest binaries are static musl;
there are no shared guest libraries, but the network-enabled build includes
ring's compiled C/assembly crypto. The harness finds a native compiler or respects
`CC_TARGET` / `AR_TARGET` variables (target triples use underscores). `rbb-mkimage --build --workspace PATH` builds a source
workspace explicitly; packaging prebuilt binaries does not require sources:

```sh
cargo build --release -p rbb-cmds --bin rbb --target x86_64-unknown-linux-musl
cargo run -p rbb-mkimage -- \
  --busybox target/x86_64-unknown-linux-musl/release/rbb \
  --file examples/initramfs/init.toml:/etc/rbb/init.toml \
  --overlay ./my-rootfs -o target/custom.cpio.gz
```

`--busybox` means the **rbb** multicall executable, not an arbitrary BusyBox.
Include additional static Rust tools (including separately built `rustbb`
multicalls) with `--file tool:/bin/tool`. ELF64 little-endian guest binaries are
supported by the packager; architecture must match the kernel. x86_64 and arm64
have both been boot-tested. Files added with `--file` are not automatically
static-checked; include their interpreters/libraries yourself if they need any.

Kernel requirements include built-in initramfs support, ELF binaries, procfs,
sysfs, devtmpfs, tmpfs, devpts, Unix datagram sockets, signalfd, serial console,
and gzip decompression for compressed images. Device drivers needed at boot must
be built in or supplied as modules. The baseline rescue tests use no disks or
networking; separate disk-root, local-kexec and network suites use disposable
images and isolated QEMU networking.

### arm64

```sh
rustup target add aarch64-unknown-linux-musl
python3 scripts/test-qemu.py --arch arm64 --kernel /path/to/arm64/Image
```

The harness cross-links using Rust's bundled `rust-lld`, boots QEMU's `virt`
machine with Cortex-A72 CPUs, uses the PL011 console (`ttyAMA0`), and checks ELF
machine type and guest `uname`. No aarch64 host userspace is required, but the TLS
backend needs a target C/assembly compiler. The harness discovers conventional
cross-compiler names or accepts `CC_aarch64_unknown_linux_musl` and optional
`AR_aarch64_unknown_linux_musl`. A Linux GNU cross compiler also worked for ring's
fixed-ABI objects in our tests; the guest libc still comes from Rust's musl target.
Raw Linux Images and EFI gzip-zboot kernels are accepted; the harness
extracts zboot's Linux Image for QEMU direct boot. It does not test UEFI boot.

For a standalone image, use `rbb-mkimage --build --target
aarch64-unknown-linux-musl` and `qemu-system-aarch64 -machine virt -cpu cortex-a72
-m 256M -kernel Image -initrd IMAGE.cpio.gz -append 'console=ttyAMA0 rdinit=/init'
-nographic -no-reboot -nic none`. With Nix's host linker wrappers, set
`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER` to the same host `rust-lld` path
shown above for x86_64.

## Services

PID 1 reads `/etc/rbb/init.toml`. Validate without mounting or starting anything:

```sh
cargo run -p rbb-init -- --check --config examples/initramfs/init.toml
```

```toml
control = "/run/rbb/control"  # default, created mode 0600
shutdown_timeout_ms = 5000   # global and per-service TERM grace period

[[services]]
name = "setup"
kind = "oneshot"
command = ["/bin/echo", "starting"]

[[services]]
name = "worker"
command = ["/bin/my-worker", "--foreground"]
after = ["setup"]
restart = "on-failure"       # always | on-failure | never
restart_delay_ms = 1000      # fixed delay, including exec failures
cwd = "/tmp"
env = { EXAMPLE = "value" }
disabled = false
```

- Services are foreground processes in separate process groups. Do not daemonize.
  Children inherit console stdin/stdout/stderr and the supervisor environment,
  with configured environment overrides and a rescue-oriented `PATH`.
- Longruns default to `restart = "always"`; oneshots default to `"never"`.
  An exec failure counts as failure (status 127). Signal exits report 128+signal.
- `after` waits for successful oneshot completion or longrun **exec/start**. It is
  **not readiness notification**, nor a continuously enforced dependency. Failed
  oneshots block dependents; stopping/restarting a dependency does not stop already
  running dependents. Duplicate names, unknown dependencies, cycles, typos and
  relative executable paths are rejected before starting services.
- PID 1 blocks signals and consumes signalfd events. Every loop drains
  `waitpid(-1, WNOHANG)`, including adopted orphans. Child signal masks are restored
  before exec. An exited service leader's remaining process group is killed.
- `ctl status`, `ctl up NAME`, `ctl down NAME`, `ctl restart NAME` control services.
  `up` on a running service is idempotent. These commands change in-memory state,
  not the TOML file. Acknowledgements mean accepted, not completed; query status.
- `poweroff` / `ctl poweroff`, or SIGTERM/SIGUSR1, initiate poweroff.
  `reboot` / `ctl reboot`, or SIGINT/SIGUSR2, initiate reboot. HUP is ignored;
  configuration reload is not implemented.
- Shutdown stops restarts, sends TERM in reverse dependency order (without waiting
  between services), then escalates to KILL at the deadline, reaps, syncs and
  powers off/reboots. PID 1 also signals escaped children in its namespace.
  After a further one-second reap budget, shutdown proceeds even if an unkillable
  process remains. `--supervise` never mounts, signals unrelated groups, or reboots
  the host. Standalone services must not escape their process groups.
- A preexisting control socket is not stolen/unlinked. Remove a stale socket
  after an unclean crash before restarting the standalone supervisor.
- Fatal PID 1 errors are printed to the console and PID 1 remains alive; there is
  no automatic rescue fallback if config parsing or early mounts fail.

For safe host experiments, set `control` to a private temporary path and run:

```sh
cargo run -p rbb-init -- --supervise --config /tmp/my-init.toml
cargo run -p rbb-cmds --bin rbb -- ctl --socket /tmp/my-control status
```

## Commands and image format

`rbb --list` lists `init ctl sh echo cat ls mkdir cp mv rm ln readlink chmod pwd
sleep true false uname mount umount insmod dmesg ps free kill poweroff reboot`.
Invocation works as `rbb echo hello` or through `/bin/echo` and `/bbin/echo`
symlinks. `mount` requires `-t TYPE` (or `-o bind`), supports a small option set,
and does no fstab probing. `insmod` accepts uncompressed `.ko` files, with no
dependency resolution. `dmesg` is read-only; `free` prints `/proc/meminfo`.

`sh` is deliberately a command interpreter, **not POSIX sh**. It supports one
command per line, single/double quotes, backslash escapes, comments, `cd`,
`export KEY=VALUE`, `exit`, script files, and `-c`. It does **not** support `$`
expansion, semicolons, pipes, redirects, conditionals, globbing or job control.
Use command arrays in service configuration rather than shell scripts for boot.
Command arguments and overlay filenames must be UTF-8.

The builder writes sorted newc records, zero uid/gid/mtime, deterministic inodes,
512-byte final padding, and gzip with zero mtime/no host filename. Ordinary file
permissions are preserved without setuid/setgid bits; hardlinks become copies.
Overlays preserve symlinks without following them. Duplicate paths (except merged
directories), traversal, symlink parents and unsupported special files are
rejected. Generated paths cannot be replaced. `init.toml` is validated at build
time. Required `/dev/console` and `/dev/null` nodes are written as CPIO records,
without calling host `mknod`. Output is atomically replaced only after assembly.
Use `--compress none` for raw CPIO. Reproducibility applies to identical input
binary/file bytes; Rust compiler path/toolchain differences may change binaries.

## Verification

```sh
cargo test --workspace
python3 -m unittest discover -s scripts -p 'test_*.py'
cargo clippy -p rbb-init -p rbb-cmds -p rbb-mkimage --all-targets -- -D warnings
python3 scripts/test-qemu.py --arch x86_64 --kernel /path/to/bzImage
python3 scripts/test-qemu.py --arch arm64 --kernel /path/to/arm64/Image
# NixOS default: --kernel /run/current-system/kernel; or set RBB_KERNEL
```

The harness builds static guest binaries, builds each image twice, compares
bytes, independently parses newc headers, and uses GNU cpio if available. It
boots gzip/poweroff and raw/reboot images under QEMU TCG. The guest checks mounts,
environment/cwd, oneshot ordering, failed dependencies, disabled services, exec
failure, restart policies, up/down/restart, adopted orphan reaping, file and
mount applets, and TERM-to-KILL escalation. The host requires guest pass markers,
completed shutdown, successful QEMU exit and no panic. A third boot builds the
default rescue image through `--build`, waits for its serial console prompt,
runs `echo`, `uname` and `ctl status`, then powers it off. Logs/images are retained
separately in `target/qemu-tests/x86_64/` and `target/qemu-tests/arm64/` (or under
`CARGO_TARGET_DIR`). Both architectures run the same guest tests.

Validated with Rust 1.99.0 and QEMU 11.1.2, x86_64 Linux 7.3 and arm64 Alpine
Linux 6.18.58-0-virt. The arm64 kernel came from
<https://dl-cdn.alpinelinux.org/alpine/v3.23/releases/aarch64/netboot/vmlinuz-virt>
and had SHA-256
`d1876369b061ef3bc33314f490240efcd32b031393cb34c6f81134fa862fe472`.
That URL is updated by Alpine; retain/check the hash when reproducing these
exact results. This suite covers initramfs boot and supervision. The separate
[disk-root tests](disk-boot.md) cover root handoff with dedicated kernels on both
architectures. [Local-kexec tests](kernel-boot.md) also reach a second kernel on
both architectures. Network boot is a subsequent increment; physical arm64 boards
have not been tested.

## Alternatives and next steps

Projects examined (upstream READMEs, not integration-tested here):

- [rinit](https://github.com/rinit-org/rinit): explicitly inspired by s6/66 and
  daemontools; oneshots, daemons and dependencies; upstream calls it work in
  progress. GPL-3.0+, README build instructions require nightly.
- [zinit](https://github.com/threefoldtech/zinit): Apache-2.0, Rust/Tokio runit-style
  PID 1, YAML service definitions and Unix-socket/JSON-RPC control.
- [Horust](https://github.com/FedericoPonzi/Horust): MIT, container-oriented
  supervisor, restart/health checking, dependency and logging configuration;
  upstream describes beta status.

For an existing Rust supervisor, zinit is worth evaluating before expanding this
implementation. This MVP keeps the guest small, synchronous, shell-independent
and directly testable; no code was copied from those projects.

Still needed for s6/u-root-level coverage: readiness fd protocol, finish hooks,
per-service logging/rotation, privilege dropping, resource limits, configuration
reload, service directories/s6 wire compatibility, automatic module discovery,
DHCP/netlink tools, classic-kexec/custom-DTB loading, archive merging, and broader
architectural/hardware testing. It is not ready to replace a production init.
