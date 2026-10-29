# rustbb - Multi-call Binary Builder for Rust

**rustbb** is a tool that combines multiple Rust CLI crates into a single multi-call binary, similar to [gobusybox](https://github.com/u-root/gobusybox) for Go. This is useful for embedded systems, containers, and other environments where minimizing binary count and size is important.

## Features

- **Fetch from anywhere** - Combine crates from local paths, crates.io, or git repositories
- **Like `cargo install`** - Just specify crate names and rustbb fetches them automatically
- **Combine existing CLI crates** - Transform unmodified Rust CLI programs into a single binary
- **Symlink and subcommand modes** - Invoke commands via symlinks (`./cat file`) or subcommands (`./busybox cat file`)
- **Automatic argument handling** - Transforms `std::env::args()` and `clap::Parser::parse()` calls
- **Async runtime support** - Handles `#[tokio::main]` and `#[async_std::main]` automatically
- **Dependency isolation** - Preserves each command's dependency sources, aliases, versions, and Cargo feature graph
- **Size optimization** - Release builds with LTO produce compact binaries

## Quick Start

```bash
# Build rustbb
cargo build -p rustbb --release

# Build from crates.io (like cargo install, but combined!)
./target/release/rustbb build hexyl bat -o tools --release

# Build from GitHub
./target/release/rustbb build github:sharkdp/fd github:sharkdp/hexyl -o finder --release

# Build from local paths
./target/release/rustbb build ./my-cli ./other-cli -o mybox --release

# Mix sources
./target/release/rustbb build ./local-tool hexyl github:user/repo -o mixed --release

# Use the combined binary
./tools hexyl file.bin       # Subcommand mode
ln -s tools hexyl && ./hexyl # Symlink mode
./tools --list               # List available commands
```

## Crate Sources

rustbb supports multiple ways to specify crates:

| Format | Description | Example |
|--------|-------------|---------|
| `./path` | Local filesystem path | `./my-cli` |
| `crate_name` | Latest version from crates.io | `hexyl` |
| `crate@version` | Cargo version requirement from crates.io (`=x.y.z` for exact) | `hexyl@0.14` |
| `github:user/repo` | GitHub repository (main branch) | `github:sharkdp/bat` |
| `github:user/repo#ref` | GitHub repository at tag/branch | `github:sharkdp/bat#v0.24.0` |
| `git:url` | Any git repository | `git:https://gitlab.com/user/repo.git` |

## How It Works

rustbb uses AST transformation (via `syn` and `quote`) to:

1. **Discover** binary targets and workspace roots using Cargo metadata
2. **Copy** each command into a separate generated library crate, preserving its feature graph
3. **Replace** `std::env::args()` / `args_os()` in the binary's module tree and supported macro arguments
4. **Transform** verified local clap `Parser` derives' `parse()` / `try_parse()` calls to explicit-argument variants
5. **Preserve** `main()` return types and async attributes, including Tokio runtime options
6. **Resolve** workspace inheritance without changing dependency sources or version constraints
7. **Generate** a dispatcher that preserves each command's `Termination` result
8. **Build** the combined binary using Cargo

### Transformation Examples

#### Simple main()
```rust
// Original
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("{}", args.join(" "));
}

// Transformed
fn main() {
    let args: Vec<String> = rustbb_runtime::args().skip(1).collect();
    println!("{}", args.join(" "));
}

pub fn rustbb_entry() -> std::process::ExitCode {
    std::process::Termination::report(main())
}
```

#### With clap
```rust
// Original
#[derive(Parser)]
struct Args { ... }

fn main() {
    let args = Args::parse();  // Uses std::env::args() internally
}

// Transformed - parse() becomes parse_from()
let args = Args::parse_from(rustbb_runtime::args_os());
```

#### Async with tokio
```rust
// Original
#[tokio::main]
async fn main() {
    do_async_stuff().await;
}

// Transformed - Tokio expands the original attribute with its original options
#[tokio::main]
async fn main() {
    do_async_stuff().await;
}

pub fn rustbb_entry() -> std::process::ExitCode {
    std::process::Termination::report(main())
}
```

## Project Structure

```
busyboxide/
├── rustbb/              # CLI tool (like gobusybox's makebb)
│   └── src/
│       ├── main.rs      # CLI entry point
│       ├── discovery.rs # Crate analysis
│       ├── transform.rs # AST transformation
│       ├── codegen.rs   # Code generation
│       └── builder.rs   # Build orchestration
├── rustbb_runtime/      # Runtime library for combined binaries
│   └── src/lib.rs       # Registry and dispatch
└── examples/            # Test fixtures
    ├── simple_echo/     # Basic echo
    ├── simple_cat/      # Basic cat
    ├── head/            # head with clap
    ├── wc/              # wc with clap
    └── async_hello/     # Async with tokio
```

## Supported Transformation Strategies

| Strategy | Status | Description |
|----------|--------|-------------|
| SimpleMain | ✅ Supported | Plain `fn main() { ... }` |
| AsyncMain (tokio) | ✅ Supported | `#[tokio::main] async fn main()` |
| AsyncMain (async_std) | ✅ Supported | `#[async_std::main] async fn main()` |
| With clap | ✅ Supported | `Args::parse()` transformed to `parse_from()` |
| LibraryInterface | ⚠️ Planned | Crates with existing `pub fn run()` |

## Comparison with gobusybox

| Aspect | gobusybox (Go) | rustbb (Rust) |
|--------|----------------|---------------|
| Transformation | AST rewrite of package/main/init | AST rewrite of main + args + clap |
| Registration | Runtime via init() | Build-time code generation |
| Argument handling | argv manipulation | Process-wide effective arguments + runtime function |
| Async support | N/A (goroutines) | ✅ tokio & async_std |
| Dependency handling | Copy to temp GOPATH | Separate command crates with preserved Cargo manifests |

## Binary Sizes (Release, stripped)

| Commands | Size |
|----------|------|
| 2 simple commands | 333KB |
| 4 commands with clap | 553KB |
| 5 commands with clap + tokio | 652KB |

## Limitations

- Global state in commands may conflict; Cargo still unifies features of shared dependency packages normally.
- Argument access in third-party libraries is not rewritten. Clap rewriting requires a verifiable local `Parser` derive or an explicitly qualified `clap::Parser` call.
- Argument access inside unsupported custom macros is rejected with a diagnostic; move the expression outside the macro. Standard formatting, assertion, `vec!`, and `dbg!` macros are supported.
- Source modules outside the package and symlinked package files are rejected rather than copied ambiguously.
- Commands with an existing library need an internally named wrapper package to avoid Cargo identity collisions; the binary's compile-time `CARGO_PKG_NAME` then reflects that wrapper name. Binary-only packages retain their original package metadata.

## Source Trust and Caching

Building Rust source can execute build scripts and procedural macros. Only build sources you trust; rustbb is not a sandbox.

Remote sources use fresh, randomly named working directories under the user's `rustbb` cache directory (or `--cache-dir`). On Unix, symlinked, foreign-owned, or group/world-writable cache locations are rejected. Legacy `/tmp/rustbb-cache` checkouts are never reused. Cargo's metadata and lockfile identify the exact registry package selected by its resolver; binary-only packages are vendored from that locked resolution and checksum-checked. Rustbb does not scan registry directories for matching names.

Workspace dependency inheritance preserves Git revisions, registries, aliases, paths, and feature settings. Unresolvable inheritance and conflicting workspace patches fail instead of falling back to crates.io or wildcard versions. Requested fetch/build failures also fail the overall build instead of silently omitting commands.

## Future Work

- [x] Async runtime support
- [x] Dependency source and feature preservation
- [ ] Library interface detection and use
- [ ] Feature flags for selective command inclusion
- [x] Cargo-managed version resolution
- [x] Integration and adversarial regression tests

## License

MIT OR Apache-2.0
