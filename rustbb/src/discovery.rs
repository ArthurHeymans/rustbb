//! Let Cargo identify targets and workspace roots; retain the original manifests.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct CrateInfo {
    pub name: String,
    pub path: PathBuf,
    pub main_path: PathBuf,
    pub library_name: Option<String>,
    pub manifest: toml::Value,
    pub workspace_root: PathBuf,
    pub workspace_manifest: toml::Value,
}

pub fn analyze_crate(path: &Path) -> Result<CrateInfo> {
    let manifest_path = path
        .join("Cargo.toml")
        .canonicalize()
        .context("Cannot locate Cargo.toml")?;
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version=1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&manifest_path)
        .output()
        .context("Failed to run cargo metadata")?;
    anyhow::ensure!(
        output.status.success(),
        "Cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let packages = metadata["packages"]
        .as_array()
        .context("Missing Cargo packages")?;
    let requested = read_manifest(&manifest_path)?;
    let package = if requested.get("package").is_some() {
        packages.iter().find(|p| {
            p["manifest_path"]
                .as_str()
                .is_some_and(|m| Path::new(m) == manifest_path)
        })
    } else {
        let defaults = metadata["workspace_default_members"]
            .as_array()
            .context("Missing workspace default members")?;
        packages
            .iter()
            .find(|p| defaults.contains(&p["id"]) && binary_target(p).is_some())
            .or_else(|| packages.iter().find(|p| binary_target(p).is_some()))
    }
    .context("No binary package found; specify a workspace binary member")?;
    let target = binary_target(package).context("This package has no binary target")?;
    let main_path = PathBuf::from(
        target["src_path"]
            .as_str()
            .context("Missing binary source path")?,
    );
    let package_manifest = PathBuf::from(
        package["manifest_path"]
            .as_str()
            .context("Missing manifest path")?,
    );
    let path = package_manifest
        .parent()
        .context("Invalid manifest path")?
        .to_path_buf();
    let workspace_root = PathBuf::from(
        metadata["workspace_root"]
            .as_str()
            .context("Missing workspace root")?,
    );
    let library_name = package["targets"]
        .as_array()
        .context("Missing targets")?
        .iter()
        .find(|t| {
            t["kind"]
                .as_array()
                .is_some_and(|k| k.iter().any(|v| matches!(v.as_str(), Some("lib" | "rlib"))))
        })
        .and_then(|t| t["name"].as_str())
        .map(str::to_owned);
    Ok(CrateInfo {
        name: package["name"]
            .as_str()
            .context("Missing package name")?
            .to_owned(),
        path,
        main_path,
        library_name,
        manifest: read_manifest(&package_manifest)?,
        workspace_manifest: read_manifest(&workspace_root.join("Cargo.toml"))?,
        workspace_root,
    })
}

fn binary_target(package: &serde_json::Value) -> Option<&serde_json::Value> {
    let targets = package["targets"].as_array()?;
    let default_run = package["default_run"].as_str();
    targets.iter().find(|t| {
        t["kind"]
            .as_array()
            .is_some_and(|k| k.iter().any(|v| v == "bin"))
            && default_run.is_none_or(|name| t["name"] == name)
    })
}

pub fn read_manifest(path: &Path) -> Result<toml::Value> {
    toml::from_str(
        &fs::read_to_string(path).with_context(|| format!("Cannot read {}", path.display()))?,
    )
    .with_context(|| format!("Cannot parse {}", path.display()))
}
