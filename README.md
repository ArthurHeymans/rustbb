# rustbb - Multi-call Binary Builder for Rust

**rustbb** is a tool that combines multiple Rust CLI crates into a single multi-call binary, similar to [gobusybox](https://github.com/u-root/gobusybox) for Go. This is useful for embedded systems, containers, and other environments where minimizing binary count and size is important.

## Features

- **Fetch from anywhere** - Combine crates from local paths, crates.io, or git repositories
- **Like `cargo install`** - Just specify crate names and rustbb fetches them automatically
- **Combine existing CLI crates** - Transform unmodified Rust CLI programs into a single binary
- **Symlink and subcommand modes** - Invoke commands via symlinks (`./cat file`) or subcommands (`./busybox cat file`)
- **Automatic argument handling** - Transforms `std::env::args()` and `clap::Parser::parse()` calls
- **Async runtime support** - Handles `#[tokio::main]` and `#[async_std::main]` automatically
- **Dependency merging** - Combines dependencies with proper version and feature handling
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
| `crate@version` | Specific version from crates.io | `hexyl@0.14` |
| `github:user/repo` | GitHub repository (main branch) | `github:sharkdp/bat` |
| `github:user/repo#ref` | GitHub repository at tag/branch | `github:sharkdp/bat#v0.24.0` |
| `git:url` | Any git repository | `git:https://gitlab.com/user/repo.git` |

## How It Works

rustbb uses AST transformation (via `syn` and `quote`) to:

1. **Analyze** each input crate's `main.rs` to determine transformation strategy
2. **Transform** the `main()` function into a callable library function
3. **Replace** calls to `std::env::args()` with `rustbb_runtime::args()` for proper argument handling
4. **Transform** `clap::Parser::parse()` to `parse_from(rustbb_runtime::args_os())`
5. **Handle async** by wrapping `#[tokio::main]` bodies with runtime builders
6. **Merge** dependencies from all crates with proper version and feature handling
7. **Generate** a combined crate with a dispatcher that routes to the correct command
8. **Build** the combined binary using cargo

### Transformation Examples

#### Simple main()
```rust
// Original
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("{}", args.join(" "));
}

// Transformed
pub fn rustbb_cmd_echo() -> i32 {
    (|| {
        let args: Vec<String> = rustbb_runtime::args().skip(1).collect();
        println!("{}", args.join(" "));
    })();
    0i32
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

// Transformed - runtime created explicitly
pub fn rustbb_cmd_async() -> i32 {
    tokio::runtime::Runtime::new()
        .expect("Failed to create Tokio runtime")
        .block_on(async {
            do_async_stuff().await;
        });
    0i32
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
| Argument handling | argv manipulation | Thread-local + runtime function |
| Async support | N/A (goroutines) | ✅ tokio & async_std |
| Dependency handling | Copy to temp GOPATH | Merge Cargo.toml with features |

## Binary Sizes (Release, stripped)

| Commands | Size |
|----------|------|
| 2 simple commands | 333KB |
| 4 commands with clap | 553KB |
| 5 commands with clap + tokio | 652KB |

## Limitations

- Global state in commands may conflict
- Some complex macro patterns may not transform correctly
- Version conflicts between dependencies use first-seen version

## Future Work

- [x] Async runtime support
- [x] Dependency version/feature merging
- [ ] Library interface detection and use
- [ ] Feature flags for selective command inclusion
- [ ] Smarter version conflict resolution
- [ ] Integration tests

## License

MIT OR Apache-2.0
