# Optional richer rescue userland

The boot-critical dispatcher remains explicit Rust code. General utilities and
shell syntax now have optional upstream implementations rather than more partial
home-grown versions:

```sh
# Small built-in userland; no uutils/brush linked.
cargo build --release -p rbb-cmds --bins --target x86_64-unknown-linux-musl
# Rich userland, still one static multicall binary.
cargo build --release -p rbb-cmds --bins --target x86_64-unknown-linux-musl \
  --features rescue-tools
```

`rescue-tools` enables `coreutils` and `brush`; either can also be enabled alone.
An image's aliases are always taken from **that target ELF's** inventory, including
its enabled features, never the host's Cargo feature selection.

`rbb-mkimage --build --preset rescue` enables `rescue-tools` by default. Other
presets default to the small build. `--build-features coreutils` or
`--build-features ''` explicitly overrides that choice. With `--busybox`, features
are already compiled into the supplied binary and cannot be changed by a preset.
The builder automatically selects Rust's bundled linker and a target C/assembly
compiler, or respects explicit `CARGO_TARGET_*_LINKER`, `CC_TARGET`, `AR_TARGET`,
`CC` and `AR` settings. It never silently uses an x86 compiler for arm64. Ring
and any enabled crypto assembly still need a suitable compiler toolchain.

## uutils

Direct `uu_*` 0.12 library calls provide:

- richer `cat`, `ls`, recursive `cp`/`rm`, `mkdir`, `ln`, `chmod`, `readlink`;
- `head`, `tail`, `wc`, `cut`, `sort`, `uniq`, `tee`, `tr`;
- `env`, `printenv`, `mktemp`, `timeout`, `dd`, `sync`;
- `mknod`, `mkfifo`, `touch`, `sha256sum`, `printf`, `test` and `[`. 

The adapter passes the real utility name/arguments and initializes uucore's
embedded localization resources. No external coreutils binary is launched.
Native ACL/SELinux/OpenSSL features are not enabled; this is not a promise of
all GNU platform integrations. The multicall front end still requires UTF-8
arguments. Without the feature, existing commands retain their documented minimal
implementations and the additional names are absent from the inventory.

## brush

With `brush`, `brush` uses the upstream shell's Bash-style mode; `sh` selects its
POSIX/sh mode. Upstream deliberately leaves `printf`, `test` and `[` external in
sh mode. The brush feature therefore includes those uutils libraries even without
the full coreutils feature, and selection of rich `sh` automatically installs
its companion aliases. Guest tests prevent host utilities from masking this gap. `minish` retains the original explicitly minimal interpreter.
The generated default console chooses `/bin/brush` when that alias is installed;
`--shell /bin/minish` is an explicit fallback. Without the feature, `sh` remains
the minimal interpreter. Preset selection does not turn a rich prebuilt binary
into the smaller build.

We reuse `brush-shell`'s public entry and its builtin/interactive/runtime setup,
with the minimal input backend (not reedline). It reads process argv itself.
Invocation performs at most one **exec-self**, not a fork or source rewrite,
to normalize argv and default to the compiled minimal input backend. Upstream's
interactive default chooses reedline even when that frontend is disabled, so the
adapter supplies an explicit supported backend. The public brush parser detects
real options (not lookalike positional arguments). `sh` supplies the upstream
`--sh` flag and preserves `$0`, positional arguments, PID and process group.
Already-normalized invocation needs no extra exec. The API is process-owning, not a reentrant
in-process shell evaluator.

Functions, command substitution, variables, loops, pipelines, redirections and
Bash arrays use brush's parser/executor. This is not a claim of complete Bash/POSIX
certification or s6 job-control semantics. The rescue console remains deliberately
**unauthenticated**. SSH/login authentication, a fully featured terminal frontend,
find/grep/archive tools and complete GNU compatibility remain separate work.

## Validation

```sh
cargo test -p rbb-cmds --features rescue-tools
python3 scripts/test-rescue-tools.py --arch x86_64
python3 scripts/test-rescue-tools.py --arch arm64
```

The guest suite checks all utility registrations, file/text/checksum behavior,
recursive copy/removal, FIFO/character-device creation, dd, timeout status, brush
pipelines/functions/arrays/expansion/redirection, exec-self PID preservation and
the minimal fallback. A second VM exercises the generated interactive brush
console over serial, including status and clean poweroff. The harness also invokes
`--build --preset minimal` and records actual small/rich executable and image
sizes under `target/rescue-tools-tests/ARCH/sizes.json`.

All optional dependency notices/source pointers are generated using locked Cargo
metadata with `--all-features`. Dependencies keep their own licenses; the source
of our adapters is not relicensed by them.
