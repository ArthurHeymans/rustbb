# Proposed u-root-style boot feature set — approval required

**Status:** proposal, not implemented or approved. The current implementation
boots its own rescue initramfs and supervises services on x86_64 and arm64.
Disk-root handoff, disk kernel boot, and network kernel boot are not present yet.

## What was inspected

This proposal follows the local `../u-root` checkout, not just its command names:

- `README.md`: the separate userland, multicall builder, initramfs builder, and
  SystemBoot layers; init/uinit selection, extra files, and cross compilation.
- `cmds/boot/boot/boot_linux.go`, `pkg/boot/localboot/localboot.go`: read-only disk
  discovery, boot-entry parsers, menu, command-line overrides, load/exec stages.
  Localboot tries BLS, GRUB, Syslinux, ISO images, and ESXi-specific paths.
- `cmds/boot/pxeboot/pxeboot_linux.go`, `pkg/boot/netboot/netboot.go`: DHCPv4/v6,
  boot URLs, HTTP/TFTP, iPXE/PXELINUX detection, direct image probing, and fallback.
- `cmds/core/switch_root/switch_root_linux.go`: handing an initramfs over to an
  already-mounted disk root. This is different from loading another kernel.
- `pkg/boot/linux/load_linux_arm64.go`: arm64 Linux handoff includes architecture
  and device-tree concerns; it is not merely the amd64 loader with another name.
- `pkg/boot/crabefi/README.md`: this checkout also has an **amd64-only** CrabEFI
  entry64 payload handoff for UEFI/Windows boot, with substantial firmware-table
  and framebuffer requirements. It is not generic EFI application execution.
- `integration/README.md` and the DHCP, PXE, kexec and UEFI test suites: meaningful
  boot tests reach a second-stage payload, not just print a parsed configuration.
- `cmds/core/`, `cmds/exp/`, `pkg/kmodule/`, `pkg/mount/`, `pkg/securelaunch/`:
  rescue tools, modules, storage, diagnostics, and specialized secure launch.

## Recommended baseline: approve A–E

### A. Boot platform and normal disk-root boot

- Kernel command-line/config parsing: root device, filesystem, mount flags,
  read-only/read-write, bounded root wait/delay, real init and its arguments.
- Device discovery through sysfs/devtmpfs, delayed devices, and UUID, PARTUUID,
  LABEL and explicit `/dev/...` selectors. GPT and MBR; ext4 and FAT first.
- Kernel module loading with `modules.dep` and aliases, dependency ordering,
  appropriate compressed-module support, and useful missing-driver errors.
  Support explicit module lists before automatic modalias handling.
- Mount the root, move `/dev`, `/proc`, `/sys`, `/run`, perform a guarded
  `switch_root`, release the old initramfs, and **exec the real init as PID 1**.
  A service child cannot perform this handoff on PID 1's behalf.
- Serial rescue fallback with visible failure reason and bounded retries.
  Never recursively remove a directory until mount and initramfs invariants have
  been checked. Discovery mounts must be read-only and must not write host disks.

This is the path needed for a conventional installed Linux system using our
initramfs instead of dracut/mkinitcpio. It does not need a second kernel.

### B. LinuxBoot-style local disk boot

- A shared `BootEntry` model: title, source, architecture, kernel, ordered initrd
  fragments, command line, optional device tree, and verification policy.
- Scan read-only partitions for BLS Type #1 entries, then a **documented subset**
  of GRUB and Syslinux/extlinux configurations. BLS first; do not execute arbitrary
  GRUB shell code or claim full GRUB compatibility.
- Serial menu, default/timeout, explicit selection, command-line append/remove/
  reuse, and inspect-only / load-without-execute modes.
- Load kernel + combined initrd + command line using `kexec_file_load` on both
  architectures where the running kernel supports it. Coordinate shutdown and
  final kexec through PID 1; loading and jumping must be separate operations.
- Check architecture, bounds and supported kernel format before handoff.
  Handle arm64 Image/device-tree requirements explicitly. Unsupported formats
  must fail clearly, not be silently interpreted as Linux kernels.
- If support for kernels without `CONFIG_KEXEC_FILE` is required, implement
  **classic `kexec_load` as a separate reviewed increment**: segment allocation,
  reserved-memory validation, bzImage/arm64 Image loading and DTB preservation.
  This is much more than a syscall wrapper and is also needed for CrabEFI.

### C. Network boot

- `ip`-style interface/address/route inspection and configuration, loopback,
  static addressing, DHCPv4 first, then DHCPv6 and IPv6/SLAAC integration.
  Install routes and DNS, support interface selection and bounded timeouts.
- Extract BOOTP server/filename and DHCP boot-server/bootfile options; send the
  correct architecture/client identification rather than pretending arm64 is PC
  BIOS PXE. Test options and overloaded fields, not just address acquisition.
- Fetch boot manifests, kernels and initrds over HTTP/HTTPS and TFTP, with size
  limits, retry limits, URL resolution, cancellation and cleanup.
- Support PXELINUX configuration discovery and a documented boot-oriented iPXE
  subset (kernel/initrd/boot and required variable/chain behavior). Do not promise
  a complete iPXE interpreter. Provide a native TOML/JSON boot manifest as well.
- Reuse the same `BootEntry`, verification and kexec machinery as local boot.
- Ordered disk/network policies and rescue fallback, with diagnostic logs.

This is a Linux initramfs acting as the network boot client, like u-root's
`pxeboot`. Producing BIOS PXE ROMs or UEFI network firmware is a different layer.
NFS/iSCSI root is not implied by downloading a kernel/initrd over the network.

### D. Practical image builder, userland and supervision

- Named minimal/rescue/diskboot/netboot presets and command selection, plus a
  shared command manifest so the host builder never has to execute an arm64
  binary to discover its applets. Keep the generic `rustbb` builder usable.
- Base CPIO reading/merging, explicit conflict policy, original-init preservation
  (`/inito`), init/uinit/shell selection, embedded boot configs, modules and trust
  roots. Preserve deterministic metadata and reject traversal/symlink escapes.
- Reuse uutils for richer file/text/archive/checksum tools rather than growing
  home-grown partial implementations. Prioritize head/tail/wc, grep/find, dd,
  sync, mknod, mkfifo, mktemp, env/printenv, timeout, sha256sum, tar/gzip, and
  improved existing commands. Keep optional commands out of the minimal image.
- Evaluate brush as the real rescue shell instead of implementing POSIX syntax
  ourselves. Retain the tiny interpreter only as an explicitly minimal option.
- Readiness notification, stop/finish hooks, structured status and log routing,
  privilege dropping, and boot-action coordination in PID 1. These are s6-style
  reliability improvements, not an assertion of s6 wire/service compatibility.
- Module/process/network diagnostics; clean unmount, reboot/poweroff/watchdog
  behavior. Include a useful emergency console without pretending it is a
  secure multi-user login service.

### E. Integrity, failure behavior and end-to-end tests

- SHA-256 checks against trusted expected digests, signed boot manifests with
  pinned public keys, and fail-closed verification. A hash downloaded from the
  same unauthenticated server is not authentication.
- TLS with certificate validation and an explicit RTC/time strategy; no automatic
  `insecure` fallback. TFTP/DHCP alone do not establish a trusted boot chain.
- Dedicated, reproducible test kernels with built-in storage/network/filesystem
  drivers and kexec support on x86_64 and arm64. Current arm64 Alpine test kernel
  has several drivers modular and does not establish kexec-file functionality.
- Test malformed/unsupported configs, bad images/digests, missing roots, delayed
  devices, DHCP timeout, failed downloads and recovery, not just happy paths.

## Existing Rust crates to reuse

The following versions/licenses were checked against crates.io metadata and,
where noted, upstream API docs. **Candidates are researched, not integration-
tested or final dependency selections.** No new boot dependencies are added by
this proposal. Pin accepted versions in Cargo.lock, audit transitive licenses,
and run static musl + QEMU checks on both architectures before adoption.

| Area | Candidate | Decision / remaining work |
|---|---|---|
| General utilities | [`uu_*`, `uucore` 0.12.0](https://github.com/uutils/coreutils), MIT | Reuse selected command libraries/APIs, not a second full coreutils binary. Check each feature's libc/native requirements and size. |
| Real shell | [`brush-core` 0.5.0 / `brush-shell` 0.4.0](https://github.com/reubeno/brush), MIT | Stronger choice than writing another shell; evaluate API integration, serial terminal behavior, dependencies and size. |
| Linux syscall wrappers | [`nix` 0.31.3](https://docs.rs/nix/0.31.3/nix/), MIT | Reuse mount, exec, signals and process primitives where practical; keep narrow audited unsafe wrappers only for gaps. |
| Link/address/route configuration | [`rtnetlink` 0.23.0](https://docs.rs/rtnetlink/0.23.0/rtnetlink/), MIT | Reuse packet/transport machinery, with a per-command runtime if needed; don't implement netlink framing ourselves. |
| DHCP client | [`mozim` 0.3.3](https://docs.rs/mozim/0.3.3/mozim/), Apache-2.0 | Existing v4/v6 client state machines. Its lease API includes raw-option access, but bootfile/BOOTP exposure and architecture options need a spike. |
| DHCP protocol fallback | [`dhcproto` 0.15.0](https://docs.rs/dhcproto/0.15.0/dhcproto/), MIT | Use if mozim cannot expose required PXE fields/options; codec only, **not** a replacement client state machine. |
| HTTP/TLS | [`ureq` 3.5.0](https://docs.rs/ureq/3.5.0/ureq/) + [`rustls` 0.23.46](https://docs.rs/rustls/0.23.46/rustls/), permissive | Prefer reusable streaming/timeouts/TLS, select crypto backend deliberately, and test static/cross build requirements. |
| TFTP | [`tftp-client` 0.3.0](https://docs.rs/tftp-client/0.3.0/tftp_client/), Apache-2.0 OR MIT | Has retries/backoff and RFC1350 transfers. Audit server transfer-ID checks, large-file block wrap, options and cancellation before using it for kernels. |
| GPT/MBR | [`gptman` 3.1.1](https://docs.rs/gptman/3.1.1/gptman/) + [`mbrman` 0.6.1](https://docs.rs/mbrman/0.6.1/mbrman/), MIT OR Apache-2.0 | Reuse read-only parsing and CRC validation; [`gpt` 4.1.0](https://docs.rs/gpt/4.1.0/gpt/) (MIT) is another candidate. Kernel mounts still handle filesystem I/O. |
| Filesystem identification | [`ext4-view` 1.0.0](https://docs.rs/ext4-view/1.0.0/ext4_view/), MIT OR Apache-2.0; [`fatfs` 0.3.6](https://docs.rs/fatfs/0.3.6/fatfs/), MIT | Evaluate for label/UUID discovery and image fixtures, not a rewrite of the kernel filesystem drivers. libblkid wrappers bring a native C dependency. |
| BLS parsing | [`boot-loader-spec` 0.1.5](https://docs.rs/boot-loader-spec/0.1.5/boot_loader_spec/), **MPL-2.0** | Existing Type #1 parser; excludes some UKI-related keys. Needs explicit license acceptance, or use a permissive alternative / small spec parser. Not a GRUB parser. |
| Kernel handoff | [`kexec-loader` 0.17.0](https://docs.rs/kexec-loader/0.17.0/kexec_loader/), MIT OR Apache-2.0 | Small, young crate; inspect `load_dry` and error handling. **File syscall only**, no classic segment/DTB loader. Do not couple download to immediate execution. |
| Image/ELF parsing | [`object` 0.40.0](https://docs.rs/object/0.40.0/object/), MIT OR Apache-2.0 | Reuse for ELF/PE structural checks where needed; not itself a Linux boot protocol or firmware loader. |
| CPIO/compression | [`cpio` 0.4.1](https://docs.rs/cpio/0.4.1/cpio/), MIT; existing `flate2`; [`lzma-rust2` 0.24.0](https://docs.rs/lzma-rust2/0.24.0/lzma_rust2/), Apache-2.0 | Reuse archive reading and compression for merging/modules; keep or replace the current tested deterministic writer only with equivalent behavior/tests. |
| Digests/signatures | [`sha2` 0.11.1](https://docs.rs/sha2/0.11.1/sha2/), MIT OR Apache-2.0; [`ed25519-dalek` 3.0.0](https://docs.rs/ed25519-dalek/3.0.0/ed25519_dalek/), BSD-3-Clause | Reuse vetted crypto; implement trust policy/manifest binding, not cryptographic primitives. |

`rtftp` 1.1.2 exists, but its GPL-3.0-or-later license and older release make it a
less attractive default than the permissively licensed client. This is a
selection consideration, not a claim that GPL use is impossible.

No clearly established Rust crate was found in this pass for the complete
GRUB/Syslinux/iPXE boot-config feature set or u-root's architecture-specific
classic-kexec/CrabEFI handoff. That is not proof none exists; check again before
implementing those increments. Small parsers/policy adapters and orchestration
are likely the genuinely project-specific work.

## Optional extensions — request separately

These are relevant u-root capabilities or deployment requirements, but should
not silently expand the first disk/netboot implementation:

1. **amd64 CrabEFI / Windows / EFI handoff**, including the entry64 ABI,
   coreboot/ACPI/SMBIOS/framebuffer preservation and classic kexec. The existing
   Rust CrabEFI payload is reusable as a payload; its Linux loader still needs
   implementation and a firmware-capable E2E test. Not an arm64 feature.
2. **FIT/device-tree image bundles**, DT overlays, booting from ISO images,
   Multiboot/ESXi, UEFI executables/UKIs, universal payloads and other image types.
   Add only with format-specific tests and an actual consumer.
3. **Encrypted/assembled/network roots**: LUKS, LVM, mdraid, btrfs subvolumes,
   NFS, iSCSI, NBD. These often require additional kernel setup/native libraries
   and are not covered by a basic ext4/FAT + network-download claim.
4. **Remote provisioning/debugging**: authenticated SSH/SCP, remote console,
   HTTP/TFTP server and provisioning workflows. Authentication/host-key policy
   must be deliberate; no unauthenticated network rescue shell.
5. **Firmware/platform tools**: PCI/SMBIOS/ACPI/DT diagnostics, IPMI, flash/SPI,
   TPM measurements/attestation, secure/measured launch. Flash writes and other
   destructive commands must never be included in the default boot policy.
6. **Host dynamic binaries**, automatic loader/shared-library collection, other
   compression formats, disk/ISO/EFI artifact production, and additional boards
   or architectures. A raw initramfs is not itself a BIOS/UEFI-bootable disk.
7. **Full s6 compatibility** or a comprehensive distribution init: compatible
   service directories, protocols/tools, users/cgroups, persistent enablement,
   dynamic reload and the wider s6 ecosystem. This is separate from boot parity.

## Definition of done / commit sequence after approval

Each increment is a buildable jj commit or small dependent stack, with unit tests
for parsers/policy and a real guest test for privileged behavior:

1. Shared boot model/policy and crate-selection spikes, with exact supported
   formats, licenses and dual-target build results recorded.
2. Device/module discovery, then root mount and PID 1 `switch_root` into an ext4
   disk image. Assert the real disk init is PID 1 and executes a unique marker.
3. File-based kexec, then BLS local boot from GPT/FAT + kernel/initrd and a second
   kernel marker. Add GRUB/extlinux subset fixtures and local menu selection.
4. Network primitives/DHCP, then PXELINUX/iPXE config fetch over TFTP/HTTP and
   actual second-kernel boot. Test explicit URL and DHCP-provided filename paths.
5. Verification and negative cases: tamper images/manifests, drop DHCP/server,
   truncate files, present invalid partition/config data, and require safe rescue.
6. Builder presets/base merging and selected uutils/brush integration; full
   regression run, artifact provenance, docs and size report.

Run disk-root, local-kexec, and network-kexec tests on **both x86_64 and arm64**.
Use disposable disks, isolated QEMU networking and local test servers, not public
netboot services or the host's disks. Use stage-specific kernel command lines
and markers so reaching the original rescue shell cannot falsely pass a second-
stage boot test. Keep serial/QEMU logs, kernel/config hashes and artifact sizes.

**Approval requested:** A–E as the main implementation scope; whether MPL-2.0
BLS parsing is acceptable; whether classic kexec is required in addition to
`kexec_file_load`; and which optional extensions, especially CrabEFI, belong in
this delivery. No disk/netboot implementation should start before that choice.
