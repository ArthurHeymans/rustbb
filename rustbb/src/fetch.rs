//! Fetch crates from various sources (crates.io, git, local paths)

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Represents the source of a crate
#[derive(Debug, Clone)]
pub enum CrateSource {
    /// Local filesystem path
    Local(PathBuf),
    /// Crate from crates.io (name, optional version)
    CratesIo {
        name: String,
        version: Option<String>,
    },
    /// Git repository (url, optional branch/tag/rev)
    Git {
        url: String,
        reference: Option<String>,
    },
}

impl CrateSource {
    /// Parse a crate source from a string
    ///
    /// Formats:
    /// - `./path/to/crate` or `/absolute/path` - Local path
    /// - `crate_name` or `crate_name@version` - crates.io
    /// - `git:url` or `git:url#branch` - Git repository
    /// - `github:user/repo` or `github:user/repo#branch` - GitHub shorthand
    pub fn parse(s: &str) -> Result<Self> {
        // Check for git: prefix
        if let Some(rest) = s.strip_prefix("git:") {
            return Self::parse_git(rest);
        }

        // Check for github: shorthand
        if let Some(rest) = s.strip_prefix("github:") {
            let url = format!("https://github.com/{}.git", rest.split('#').next().unwrap());
            let reference = rest.split('#').nth(1).map(|s| s.to_string());
            return Ok(CrateSource::Git { url, reference });
        }

        // Check for gitlab: shorthand
        if let Some(rest) = s.strip_prefix("gitlab:") {
            let url = format!("https://gitlab.com/{}.git", rest.split('#').next().unwrap());
            let reference = rest.split('#').nth(1).map(|s| s.to_string());
            return Ok(CrateSource::Git { url, reference });
        }

        // Check if it looks like a path (contains / or . at start, or is absolute)
        if s.starts_with('/')
            || s.starts_with("./")
            || s.starts_with("../")
            || s.contains('/')
            || Path::new(s).exists()
        {
            return Ok(CrateSource::Local(PathBuf::from(s)));
        }

        // Otherwise, treat as crates.io crate
        Self::parse_crates_io(s)
    }

    fn parse_git(s: &str) -> Result<Self> {
        let (url, reference) = if let Some(idx) = s.find('#') {
            (&s[..idx], Some(s[idx + 1..].to_string()))
        } else {
            (s, None)
        };

        Ok(CrateSource::Git {
            url: url.to_string(),
            reference,
        })
    }

    fn parse_crates_io(s: &str) -> Result<Self> {
        if let Some(idx) = s.find('@') {
            let name = s[..idx].to_string();
            let version = Some(s[idx + 1..].to_string());
            Ok(CrateSource::CratesIo { name, version })
        } else {
            Ok(CrateSource::CratesIo {
                name: s.to_string(),
                version: None,
            })
        }
    }

    /// Get a display name for this source
    pub fn display_name(&self) -> String {
        match self {
            CrateSource::Local(path) => path.display().to_string(),
            CrateSource::CratesIo { name, version } => {
                if let Some(v) = version {
                    format!("{}@{}", name, v)
                } else {
                    name.clone()
                }
            }
            CrateSource::Git { url, reference } => {
                if let Some(r) = reference {
                    format!("{}#{}", url, r)
                } else {
                    url.clone()
                }
            }
        }
    }
}

/// Fetched crate ready for processing
#[allow(dead_code)]
pub struct FetchedCrate {
    pub name: String,
    pub path: PathBuf,
    pub source: CrateSource,
    /// If true, the path is temporary and should be cleaned up
    pub is_temporary: bool,
}

/// Fetch a crate from its source to a local path
pub fn fetch_crate(source: &CrateSource, cache_dir: &Path) -> Result<FetchedCrate> {
    match source {
        CrateSource::Local(path) => fetch_local(path, source),
        CrateSource::CratesIo { name, version } => {
            fetch_from_crates_io(name, version.as_deref(), cache_dir, source)
        }
        CrateSource::Git { url, reference } => {
            fetch_from_git(url, reference.as_deref(), cache_dir, source)
        }
    }
}

fn fetch_local(path: &Path, source: &CrateSource) -> Result<FetchedCrate> {
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    if !abs_path.exists() {
        anyhow::bail!("Path does not exist: {}", abs_path.display());
    }

    // Get crate name from Cargo.toml
    let cargo_toml = abs_path.join("Cargo.toml");
    let name = if cargo_toml.exists() {
        let manifest = cargo_toml::Manifest::from_path(&cargo_toml)?;
        manifest
            .package
            .map(|p| p.name)
            .unwrap_or_else(|| path.file_name().unwrap().to_string_lossy().to_string())
    } else {
        path.file_name().unwrap().to_string_lossy().to_string()
    };

    Ok(FetchedCrate {
        name,
        path: abs_path,
        source: source.clone(),
        is_temporary: false,
    })
}

fn fetch_from_crates_io(
    name: &str,
    version: Option<&str>,
    cache_dir: &Path,
    source: &CrateSource,
) -> Result<FetchedCrate> {
    println!("  Fetching {} from crates.io...", name);

    // Use cargo to download the crate
    // First, create a temporary Cargo.toml to fetch the crate
    let fetch_dir = cache_dir.join(format!("fetch-{}", name));
    std::fs::create_dir_all(&fetch_dir)?;

    let dep_spec = if let Some(v) = version {
        format!("{} = \"{}\"", name, v)
    } else {
        format!("{} = \"*\"", name)
    };

    let cargo_toml = format!(
        r#"[package]
name = "fetch-{name}"
version = "0.0.0"
edition = "2021"

[dependencies]
{dep_spec}
"#
    );

    std::fs::write(fetch_dir.join("Cargo.toml"), cargo_toml)?;
    std::fs::create_dir_all(fetch_dir.join("src"))?;
    std::fs::write(fetch_dir.join("src/lib.rs"), "")?;

    // Run cargo fetch to download
    let output = Command::new("cargo")
        .args(["fetch"])
        .current_dir(&fetch_dir)
        .output()
        .context("Failed to run cargo fetch")?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to fetch crate {}: {}",
            name,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Find the downloaded crate in the cargo cache
    let cargo_home = std::env::var("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap().join(".cargo"));

    let registry_src = cargo_home.join("registry/src");

    // Find the crate directory (it will be in a subdirectory named after the registry)
    let crate_path = find_crate_in_registry(&registry_src, name, version)?;

    // Copy to our cache directory for modification
    let dest_dir = cache_dir.join(format!("crate-{}", name));
    if dest_dir.exists() {
        std::fs::remove_dir_all(&dest_dir)?;
    }
    copy_dir_all(&crate_path, &dest_dir)?;

    // Clean up fetch directory
    let _ = std::fs::remove_dir_all(&fetch_dir);

    Ok(FetchedCrate {
        name: name.to_string(),
        path: dest_dir,
        source: source.clone(),
        is_temporary: true,
    })
}

fn find_crate_in_registry(
    registry_src: &Path,
    name: &str,
    version: Option<&str>,
) -> Result<PathBuf> {
    // Registry sources are in subdirectories like "index.crates.io-6f17d22bba15001f"
    for entry in std::fs::read_dir(registry_src)? {
        let entry = entry?;
        let registry_dir = entry.path();
        if !registry_dir.is_dir() {
            continue;
        }

        // Look for crate directories matching the pattern: name-version
        for crate_entry in std::fs::read_dir(&registry_dir)? {
            let crate_entry = crate_entry?;
            let crate_dir = crate_entry.path();
            let dir_name = crate_dir.file_name().unwrap().to_string_lossy();

            // Check if this directory matches our crate
            if dir_name.starts_with(&format!("{}-", name)) {
                // Extract version from directory name
                let dir_version = &dir_name[name.len() + 1..];

                // If a specific version was requested, check it matches
                if let Some(v) = version {
                    if dir_version == v {
                        return Ok(crate_dir);
                    }
                } else {
                    // No specific version, return the first match (could improve to get latest)
                    return Ok(crate_dir);
                }
            }
        }
    }

    anyhow::bail!("Could not find crate {} in registry cache", name)
}

fn fetch_from_git(
    url: &str,
    reference: Option<&str>,
    cache_dir: &Path,
    source: &CrateSource,
) -> Result<FetchedCrate> {
    println!("  Cloning {}...", url);

    // Create a unique directory name from the URL
    let url_hash = simple_hash(url);
    let clone_dir = cache_dir.join(format!("git-{}", url_hash));

    if clone_dir.exists() {
        // Update existing clone
        let mut cmd = Command::new("git");
        cmd.args(["fetch", "--all"]).current_dir(&clone_dir);
        cmd.output()?;
    } else {
        // Fresh clone
        let mut cmd = Command::new("git");
        cmd.args(["clone", "--depth=1"]);

        if let Some(r) = reference {
            cmd.args(["--branch", r]);
        }

        cmd.arg(url).arg(&clone_dir);

        let output = cmd.output().context("Failed to run git clone")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to clone {}: {}",
                url,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    // If reference specified and not already on it, checkout
    if let Some(r) = reference {
        let output = Command::new("git")
            .args(["checkout", r])
            .current_dir(&clone_dir)
            .output()?;

        if !output.status.success() {
            anyhow::bail!("Failed to checkout {}", r);
        }
    }

    // Get crate name from Cargo.toml
    let cargo_toml = clone_dir.join("Cargo.toml");
    let name = if cargo_toml.exists() {
        let manifest = cargo_toml::Manifest::from_path(&cargo_toml)?;
        manifest
            .package
            .map(|p| p.name)
            .unwrap_or_else(|| extract_repo_name(url))
    } else {
        extract_repo_name(url)
    };

    Ok(FetchedCrate {
        name,
        path: clone_dir,
        source: source.clone(),
        is_temporary: true,
    })
}

fn extract_repo_name(url: &str) -> String {
    url.trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("unknown")
        .to_string()
}

fn simple_hash(s: &str) -> String {
    // Simple hash for creating unique directory names
    let mut hash: u64 = 0;
    for byte in s.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte as u64);
    }
    format!("{:016x}", hash)
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_local_path() {
        let source = CrateSource::parse("./examples/echo").unwrap();
        assert!(matches!(source, CrateSource::Local(_)));
    }

    #[test]
    fn test_parse_crates_io() {
        let source = CrateSource::parse("clap").unwrap();
        assert!(matches!(
            source,
            CrateSource::CratesIo {
                name,
                version: None
            } if name == "clap"
        ));

        let source = CrateSource::parse("clap@4.0").unwrap();
        assert!(matches!(
            source,
            CrateSource::CratesIo {
                name,
                version: Some(v)
            } if name == "clap" && v == "4.0"
        ));
    }

    #[test]
    fn test_parse_git() {
        let source = CrateSource::parse("git:https://github.com/user/repo.git").unwrap();
        assert!(
            matches!(source, CrateSource::Git { url, reference: None } if url.contains("github"))
        );

        let source = CrateSource::parse("git:https://github.com/user/repo.git#main").unwrap();
        assert!(matches!(source, CrateSource::Git { url: _, reference: Some(r) } if r == "main"));
    }

    #[test]
    fn test_parse_github_shorthand() {
        let source = CrateSource::parse("github:user/repo").unwrap();
        assert!(
            matches!(source, CrateSource::Git { url, reference: None } if url == "https://github.com/user/repo.git")
        );

        let source = CrateSource::parse("github:user/repo#v1.0").unwrap();
        assert!(matches!(source, CrateSource::Git { url: _, reference: Some(r) } if r == "v1.0"));
    }
}
