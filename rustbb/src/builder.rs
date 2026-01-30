use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

use crate::codegen::{generate_combined_crate, GeneratedCrate};
use crate::discovery::{analyze_crate, CrateInfo, TransformStrategy};
use crate::transform::{sanitize_name, transform_main};

pub fn build(crate_paths: &[PathBuf], output_name: &str, release: bool) -> Result<()> {
    // Step 1: Analyze all crates
    println!("Analyzing crates...");
    let crate_infos: Vec<CrateInfo> = crate_paths
        .iter()
        .map(|p| {
            let abs_path = if p.is_absolute() {
                p.clone()
            } else {
                std::env::current_dir()?.join(p)
            };
            analyze_crate(&abs_path)
        })
        .collect::<Result<Vec<_>>>()?;

    // Step 2: Transform each crate
    println!("Transforming {} crates...", crate_infos.len());
    let mut transformed = HashMap::new();

    for info in &crate_infos {
        match &info.strategy {
            TransformStrategy::SimpleMain => {
                let source = fs::read_to_string(&info.main_path)?;
                let transformed_source = transform_main(&source, &info.name)?;
                transformed.insert(info.name.clone(), transformed_source);
                println!("  ✓ {}", info.name);
            }
            TransformStrategy::AttributedMain { attrs } => {
                // Check if it's a supported async runtime
                let is_supported_async = attrs
                    .iter()
                    .any(|a| matches!(a.as_str(), "tokio::main" | "async_std::main"));

                // Check if all attributes are "harmless" (don't affect execution)
                let harmless_attrs = [
                    "allow",
                    "warn",
                    "deny",
                    "forbid",
                    "cfg",
                    "cfg_attr",
                    "inline",
                    "cold",
                    "must_use",
                    "track_caller",
                ];
                let all_harmless = attrs.iter().all(|a| {
                    harmless_attrs.contains(&a.as_str())
                        || matches!(a.as_str(), "tokio::main" | "async_std::main")
                });

                if is_supported_async || all_harmless {
                    let source = fs::read_to_string(&info.main_path)?;
                    match transform_main(&source, &info.name) {
                        Ok(transformed_source) => {
                            transformed.insert(info.name.clone(), transformed_source);
                            let suffix = if is_supported_async { " (async)" } else { "" };
                            println!("  ✓ {}{}", info.name, suffix);
                        }
                        Err(e) => {
                            println!("  ✗ {} - transform failed: {}", info.name, e);
                        }
                    }
                } else {
                    let unsupported: Vec<_> = attrs
                        .iter()
                        .filter(|a| {
                            !harmless_attrs.contains(&a.as_str())
                                && !matches!(a.as_str(), "tokio::main" | "async_std::main")
                        })
                        .collect();
                    println!(
                        "  ⚠ {} - unsupported attributes {:?}",
                        info.name, unsupported
                    );
                }
            }
            TransformStrategy::Unsupported { reason } => {
                println!("  ✗ {} - {}", info.name, reason);
            }
            TransformStrategy::LibraryInterface { .. } => {
                println!("  ⚠ {} - library interface not yet supported", info.name);
            }
        }
    }

    // Filter to only successfully transformed crates
    let valid_crates: Vec<CrateInfo> = crate_infos
        .into_iter()
        .filter(|c| transformed.contains_key(&c.name))
        .collect();

    if valid_crates.is_empty() {
        anyhow::bail!("No crates could be transformed");
    }

    // Step 3: Generate combined crate
    println!("Generating combined crate...");

    // Find rustbb_runtime path
    let runtime_path = find_runtime_path()?;

    let generated = generate_combined_crate(
        &valid_crates,
        output_name,
        &transformed,
        &runtime_path.display().to_string(),
    )?;

    // Step 4: Write to temp directory
    let temp_dir = TempDir::new()?;
    write_generated_crate(&temp_dir, &generated)?;

    println!("  Generated in: {}", temp_dir.path().display());

    // Step 5: Build with cargo
    println!("Building{}...", if release { " (release)" } else { "" });
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    if release {
        cmd.arg("--release");
    }
    cmd.current_dir(temp_dir.path());

    let output = cmd.output().context("Failed to run cargo build")?;

    if !output.status.success() {
        eprintln!("Cargo build failed:");
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        anyhow::bail!("Cargo build failed");
    }

    // Step 6: Copy output binary
    let build_dir = if release { "release" } else { "debug" };
    let binary_path = temp_dir
        .path()
        .join("target")
        .join(build_dir)
        .join(output_name);

    let output_path = PathBuf::from(output_name);
    fs::copy(&binary_path, &output_path)
        .with_context(|| format!("Failed to copy binary from {:?}", binary_path))?;

    // Make executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&output_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&output_path, perms)?;
    }

    println!("✓ Built: {}", output_path.display());
    println!(
        "  Commands: {:?}",
        valid_crates.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    println!();
    println!("Usage:");
    println!(
        "  ./{} <command> [args...]     # Subcommand mode",
        output_name
    );
    println!(
        "  ln -s {} <command>           # Create symlink",
        output_name
    );
    println!("  ./<command> [args...]         # Symlink mode");

    Ok(())
}

fn write_generated_crate(temp_dir: &TempDir, generated: &GeneratedCrate) -> Result<()> {
    let base = temp_dir.path();

    // Write Cargo.toml
    fs::write(base.join("Cargo.toml"), &generated.cargo_toml)?;

    // Create src directory
    let src_dir = base.join("src");
    fs::create_dir_all(&src_dir)?;

    // Write main.rs
    fs::write(src_dir.join("main.rs"), &generated.main_rs)?;

    // Write command modules
    for (name, source) in &generated.command_modules {
        let sanitized = sanitize_name(name);
        fs::write(src_dir.join(format!("{}.rs", sanitized)), source)?;
    }

    // Copy .cargo/config.toml if it exists (for linker settings, etc.)
    copy_cargo_config(base)?;

    Ok(())
}

fn copy_cargo_config(dest_dir: &Path) -> Result<()> {
    // Look for .cargo/config.toml in current directory or parent directories
    let mut search_dir = std::env::current_dir()?;

    loop {
        let cargo_config = search_dir.join(".cargo").join("config.toml");
        if cargo_config.exists() {
            let dest_cargo_dir = dest_dir.join(".cargo");
            fs::create_dir_all(&dest_cargo_dir)?;
            fs::copy(&cargo_config, dest_cargo_dir.join("config.toml"))?;
            return Ok(());
        }

        // Also check for config (without .toml extension)
        let cargo_config_alt = search_dir.join(".cargo").join("config");
        if cargo_config_alt.exists() {
            let dest_cargo_dir = dest_dir.join(".cargo");
            fs::create_dir_all(&dest_cargo_dir)?;
            fs::copy(&cargo_config_alt, dest_cargo_dir.join("config"))?;
            return Ok(());
        }

        // Move to parent directory
        if let Some(parent) = search_dir.parent() {
            search_dir = parent.to_path_buf();
        } else {
            break;
        }
    }

    // No cargo config found, which is fine
    Ok(())
}

fn find_runtime_path() -> Result<PathBuf> {
    // Try to find rustbb_runtime relative to the executable
    // In development, use workspace path

    // First, try CARGO_MANIFEST_DIR (set during cargo build/run)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let runtime_path = PathBuf::from(manifest_dir)
            .parent()
            .map(|p| p.join("rustbb_runtime"))
            .filter(|p| p.exists());

        if let Some(path) = runtime_path {
            return Ok(path);
        }
    }

    // Try relative to current executable
    if let Ok(exe_path) = std::env::current_exe() {
        // Assume layout: target/debug/rustbb -> ../../rustbb_runtime
        let runtime_path = exe_path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|p| p.join("rustbb_runtime"))
            .filter(|p| p.exists());

        if let Some(path) = runtime_path {
            return Ok(path);
        }
    }

    // Try current directory
    let cwd_runtime = Path::new("rustbb_runtime");
    if cwd_runtime.exists() {
        return Ok(cwd_runtime.to_path_buf().canonicalize()?);
    }

    // Try parent of current directory (if running from within workspace)
    let parent_runtime = Path::new("../rustbb_runtime");
    if parent_runtime.exists() {
        return Ok(parent_runtime.to_path_buf().canonicalize()?);
    }

    anyhow::bail!(
        "Could not find rustbb_runtime. Make sure you're running from the workspace directory \
         or that rustbb_runtime is in the same directory as rustbb."
    )
}
