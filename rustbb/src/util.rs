//! Shared utility functions used across rustbb modules.

use anyhow::Result;
use std::fs;
use std::path::Path;

/// Copy `.cargo/config.toml` (or `.cargo/config`) from the current directory
/// or parent directories to the given destination directory.
///
/// This is needed for NixOS and other environments with custom linker settings.
/// Returns Ok(()) if no config is found (that's fine — most setups don't have one).
pub fn copy_cargo_config(dest_dir: &Path) -> Result<()> {
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
