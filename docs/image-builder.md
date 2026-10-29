# Image presets and base archives

`rbb-mkimage` reads the versioned `.rbb.commands` ELF section from the supplied
static rbb binary. It never executes that binary, including arm64 binaries on an
x86_64 builder. The inventory's architecture must agree with the ELF header;
missing, oversized, unsupported or malformed inventories fail closed. Rebuild
older pre-inventory rbb executables before packaging them.

```sh
cargo run -p rbb-mkimage -- --busybox target/aarch64-unknown-linux-musl/release/rbb \
  --preset netboot --file boot.toml:/etc/rbb/init.toml -o netboot.cpio.gz
```

Presets select installed aliases:

- `minimal`: PID 1/control, minimal shell and basic file/mount/process/rescue tools.
- `rescue` (default): every command actually advertised by the target binary.
- `diskboot`: minimal plus storage/module and local-kexec commands.
- `netboot`: minimal plus interface/DHCP/download/network-kexec commands.

`--commands init,ctl,sh,echo,poweroff` explicitly selects aliases instead of a
preset; unavailable, duplicate or unsafe names are rejected. Init is mandatory.
For a rich binary, selecting `sh` also installs its required `printf`, `test` and
`[` companions. The installed selection is recorded at `/etc/rbb/commands.json`. This controls
aliases, **not security or code stripping**: invoking an unaliased compiled command
via `/bbin/rbb` is still possible. Optional compile features, not alias removal,
control whether optional dependencies are linked into a binary. With `--build`,
rescue defaults to `rescue-tools`; other presets default to the small build.
`--build-features` overrides this choice; see [richer userland](rescue-userland.md).

## Merge bases and preserve init

```sh
cargo run -p rbb-mkimage -- --busybox target/x86_64-unknown-linux-musl/release/rbb \
  --base base.cpio.gz --preserve-init --preset rescue \
  --file my-uinit:/bin/my-uinit --uinit /bin/my-uinit \
  --shell /bin/sh -o combined.cpio.gz
```

Repeat `--base` to merge archives in order. Inputs may contain raw newc/CRC
archives, gzip members, or concatenated raw/gzip archives with zero padding.
Each gzip member must contain complete CPIO archives; other compression formats
and split-archive gzip members are not supported yet. gzip checksums and CPIO
CRC checksums are verified. The implementation reuses `cpio`'s reader, with a
header guard **before** its namesize allocation.

- `--conflict error` (default) rejects duplicate non-directory paths.
- `--conflict keep` retains the first entry.
- `--conflict replace` uses the later entry. It cannot replace a nonempty
  directory with a file or symlink. Directory-directory collisions merge children;
  keep/error preserve the first directory metadata, replace uses the later metadata.
- Bases come first, generated entries second, then overlays and `--file` mappings.
  Overlays may not replace builder-managed binary, inventory, identity or notices.
- `--preserve-init` moves an original base `/init` to `/inito` before adding our init.
  A preexisting `/inito` is an error; it is never silently overwritten.
- `--init /some/image/executable` explicitly selects a different PID 1 executable.
- `--shell` and `--uinit` configure the generated console/preceding oneshot service.
  They conflict with an already-supplied init.toml rather than silently doing nothing.
  An original `/inito` is **not** automatically executed: it may require PID 1 and
  therefore be inappropriate as a child. Use `--init /inito` if that is intended.

Names and parent relationships are validated in memory, without host extraction,
mounts, chown or mknod. Traversal, NUL paths, unsupported entry types, symlink
parents and attempts to replace populated directories are rejected. Bases with
merged-/usr directory symlinks must currently be flattened first; we deliberately
reject them rather than follow directory aliases during extraction. Selected
init/shell/uinit paths use a bounded, image-root-confined symlink resolver.

Regular files, directories, symlinks, FIFOs and character/block device metadata
are supported. Base ownership and permission bits are retained. Hardlink groups
are checked and materialized as independent files before merge, avoiding inode
aliasing across layers; this can increase image size. Mtime, inode allocation and
archive padding are normalized. Host overlays keep the original root-owned,
non-setuid file policy. Compression headers are deterministic.

Archive-import limits: 256 MiB input/expanded/materialized data, 128 MiB per file,
4097 bytes per encoded pathname, and 65536 CPIO headers. Import and merge hold data
in memory; peak memory is a multiple of the data limit. The output is constructed
and checked before atomic replacement, so a failed import/merge leaves an
existing requested output untouched. Base archives and custom executable payloads
and stable host overlays are trusted build inputs, not a code-execution sandbox.

## Tests

```sh
cargo test -p rbb-manifest -p rbb-mkimage
python3 scripts/test-image-builder.py --arch x86_64
python3 scripts/test-image-builder.py --arch arm64
```

Four presets per architecture boot real images containing independently-produced
CRC/raw/gzip bases. Guests verify ownership, FIFO/device metadata, materialized
hardlink and symlink contents, a fresh base marker, preserved `/inito`, uinit-before-
console ordering and the installed alias selection. Each merged image is built
twice and compared byte-for-byte. No host root privileges are used.
