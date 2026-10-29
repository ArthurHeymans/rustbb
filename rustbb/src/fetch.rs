//! Fetch sources into private, per-request directories. Never execute a cached checkout.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

#[derive(Debug, Clone)]
pub enum CrateSource {
    Local(PathBuf),
    CratesIo {
        name: String,
        version: Option<String>,
    },
    Git {
        url: String,
        reference: Option<String>,
    },
}

impl CrateSource {
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(rest) = s.strip_prefix("git:") {
            return Self::parse_git(rest);
        }
        for (prefix, host) in [("github:", "github.com"), ("gitlab:", "gitlab.com")] {
            if let Some(rest) = s.strip_prefix(prefix) {
                let (repo, reference) = rest.split_once('#').unwrap_or((rest, ""));
                let url = format!("https://{host}/{repo}.git");
                return Self::parse_git(&format!("{url}#{reference}"));
            }
        }
        if s.starts_with('.') || s.contains('/') || Path::new(s).exists() {
            return Ok(Self::Local(PathBuf::from(s)));
        }
        let (name, version) = s
            .split_once('@')
            .map_or((s, None), |(n, v)| (n, Some(v.to_owned())));
        anyhow::ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid crates.io package name: {name:?}"
        );
        anyhow::ensure!(version.as_deref() != Some(""), "Empty version requirement");
        Ok(Self::CratesIo {
            name: name.to_owned(),
            version,
        })
    }

    fn parse_git(s: &str) -> Result<Self> {
        let (url, reference) = s.split_once('#').unwrap_or((s, ""));
        anyhow::ensure!(!url.is_empty() && !url.starts_with('-'), "Invalid Git URL");
        anyhow::ensure!(!reference.starts_with('-'), "Invalid Git reference");
        Ok(Self::Git {
            url: url.to_owned(),
            reference: (!reference.is_empty()).then(|| reference.to_owned()),
        })
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::CratesIo { name, version } => version
                .as_ref()
                .map_or_else(|| name.clone(), |v| format!("{name}@{v}")),
            Self::Git { url, reference } => reference
                .as_ref()
                .map_or_else(|| url.clone(), |r| format!("{url}#{r}")),
        }
    }
}

pub struct FetchedCrate {
    pub name: String,
    pub path: PathBuf,
    // Keep a fresh Git checkout alive until the complete build finishes.
    _temporary: Option<TempDir>,
}

pub fn default_cache_dir() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("Cannot locate the user's cache directory; use --cache-dir")?
        .join("rustbb"))
}

/// Refuse symlinks, foreign-owned directories, and writable shared ancestors.
/// The root-owned sticky system temporary directory is a safe ancestor, not a cache root.
pub fn prepare_cache_dir(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        if let Err(e) = fs::symlink_metadata(ancestor) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(e.into());
            }
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            if let Err(e) = builder.create(ancestor) {
                // Concurrent invocations may create the same cache root. Always
                // validate what won the race, rather than trusting AlreadyExists.
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(e.into());
                }
            }
        }
        let meta = fs::symlink_metadata(ancestor)?;
        anyhow::ensure!(
            !meta.file_type().is_symlink() && meta.is_dir(),
            "Unsafe cache directory: {}",
            ancestor.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid has no preconditions or side effects.
            let uid = unsafe { libc::geteuid() };
            let sticky_root = meta.uid() == 0 && meta.mode() & 0o1000 != 0 && ancestor != path;
            anyhow::ensure!(
                (meta.uid() == uid || (meta.uid() == 0 && ancestor != path))
                    && (meta.mode() & 0o022 == 0 || sticky_root),
                "Cache directory must be privately owned and not group/world writable: {}",
                ancestor.display()
            );
        }
    }
    Ok(path)
}

pub fn fetch_crate(source: &CrateSource, cache_dir: &Path) -> Result<FetchedCrate> {
    match source {
        CrateSource::Local(path) => fetched(
            path.canonicalize()
                .with_context(|| format!("Invalid crate path: {}", path.display()))?,
            None,
        ),
        CrateSource::CratesIo { name, version } => {
            fetch_from_crates_io(name, version.as_deref(), cache_dir)
        }
        CrateSource::Git { url, reference } => fetch_from_git(url, reference.as_deref(), cache_dir),
    }
}

fn fetched(path: PathBuf, temporary: Option<TempDir>) -> Result<FetchedCrate> {
    let manifest = cargo_toml::Manifest::from_path(path.join("Cargo.toml"))?;
    let name = manifest.package.map(|p| p.name).unwrap_or_else(|| {
        path.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    Ok(FetchedCrate {
        name,
        path,
        _temporary: temporary,
    })
}

fn fetch_from_crates_io(
    name: &str,
    version: Option<&str>,
    cache_dir: &Path,
) -> Result<FetchedCrate> {
    let cache_dir = prepare_cache_dir(cache_dir)?;
    let resolver = TempDir::new_in(cache_dir)?;
    let manifest = toml::toml! {
        [package]
        name = "rustbb-source-resolver"
        version = "0.0.0"
        edition = "2021"
        [workspace]
        [dependencies.rustbb_source]
        package = name
        version = (version.unwrap_or("*"))
    };
    fs::write(
        resolver.path().join("Cargo.toml"),
        toml::to_string(&manifest)?,
    )?;
    fs::create_dir(resolver.path().join("src"))?;
    fs::write(resolver.path().join("src/lib.rs"), "")?;
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1"])
        .current_dir(resolver.path())
        .output()
        .context("Failed to run cargo metadata")?;
    anyhow::ensure!(
        output.status.success(),
        "Failed to fetch {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    if let Some(crate_path) = resolved_registry_path(&metadata, name)? {
        return fetched(crate_path, None);
    }
    // Cargo deliberately omits binary-only dependencies from metadata's graph,
    // but keeps their exact identity in Cargo.lock. Vendor that locked graph;
    // never guess a registry cache path or substitute a similarly named crate.
    let lock: toml::Value =
        toml::from_str(&fs::read_to_string(resolver.path().join("Cargo.lock"))?)?;
    let (version, checksum) = locked_registry_package(&lock, name)?;
    let output = Command::new("cargo")
        .args([
            "vendor",
            "--locked",
            "--versioned-dirs",
            "--respect-source-config",
            "vendor",
        ])
        .current_dir(resolver.path())
        .output()
        .context("Failed to vendor binary-only package")?;
    anyhow::ensure!(
        output.status.success(),
        "Failed to vendor {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let crate_path = resolver
        .path()
        .join("vendor")
        .join(format!("{name}-{version}"));
    let checksums: serde_json::Value =
        serde_json::from_slice(&fs::read(crate_path.join(".cargo-checksum.json"))?)?;
    anyhow::ensure!(
        checksums["package"].as_str() == Some(checksum.as_str()),
        "Vendored package checksum differs from Cargo's selected package"
    );
    // This is our private, checksum-verified copy, not Cargo's source cache.
    // Detach it from any enclosing workspace before asking Cargo to analyze it.
    let manifest_path = crate_path.join("Cargo.toml");
    let mut manifest: toml::Value = toml::from_str(&fs::read_to_string(&manifest_path)?)?;
    if let Some(package) = manifest
        .get_mut("package")
        .and_then(toml::Value::as_table_mut)
    {
        package.remove("workspace");
    }
    manifest
        .as_table_mut()
        .context("Invalid vendored manifest")?
        .entry("workspace")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    fs::write(manifest_path, toml::to_string(&manifest)?)?;
    fetched(crate_path, Some(resolver))
}

/// Follow the resolver root's dependency edge, never scan registry directory names.
fn resolved_registry_path(metadata: &serde_json::Value, name: &str) -> Result<Option<PathBuf>> {
    let resolve = &metadata["resolve"];
    let root = resolve["root"]
        .as_str()
        .context("Cargo metadata has no resolver root")?;
    let node = resolve["nodes"]
        .as_array()
        .context("Cargo metadata has no nodes")?
        .iter()
        .find(|n| n["id"].as_str() == Some(root))
        .context("Missing resolver root node")?;
    let Some(dependency) = node["deps"]
        .as_array()
        .context("Missing root dependencies")?
        .iter()
        .find(|d| d["name"] == "rustbb_source")
    else {
        return Ok(None);
    };
    let package_id = dependency["pkg"]
        .as_str()
        .context("Missing resolved package ID")?;
    let package = metadata["packages"]
        .as_array()
        .context("Missing Cargo packages")?
        .iter()
        .find(|p| p["id"].as_str() == Some(package_id))
        .context("Missing resolved package")?;
    anyhow::ensure!(
        package["name"] == name,
        "Resolved package name differs from requested package"
    );
    anyhow::ensure!(
        package["source"]
            .as_str()
            .is_some_and(|s| s.starts_with("registry+")),
        "Requested package is not from a registry"
    );
    let manifest = Path::new(
        package["manifest_path"]
            .as_str()
            .context("Missing resolved manifest")?,
    );
    Ok(Some(
        manifest
            .parent()
            .context("Invalid resolved manifest path")?
            .to_path_buf(),
    ))
}

fn locked_registry_package(lock: &toml::Value, name: &str) -> Result<(String, String)> {
    let packages = lock["package"]
        .as_array()
        .context("Missing lockfile packages")?;
    let root = packages
        .iter()
        .find(|p| p["name"].as_str() == Some("rustbb-source-resolver") && p.get("source").is_none())
        .context("Missing resolver lockfile entry")?;
    let id = root["dependencies"]
        .as_array()
        .context("Missing locked dependency")?
        .iter()
        .filter_map(toml::Value::as_str)
        .find(|d| d.split_whitespace().next() == Some(name))
        .context("Requested package missing from Cargo.lock")?;
    let mut id = id.split_whitespace();
    id.next();
    let version = id.next();
    let source = id.next().map(|s| s.trim_matches(['(', ')']));
    let candidates: Vec<_> = packages
        .iter()
        .filter(|p| {
            p["name"].as_str() == Some(name)
                && p.get("source")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|s| s.starts_with("registry+") && source.is_none_or(|v| s == v))
                && version.is_none_or(|v| p["version"].as_str() == Some(v))
        })
        .collect();
    anyhow::ensure!(
        candidates.len() == 1,
        "Ambiguous or non-registry locked package {name}"
    );
    let package = candidates[0];
    Ok((
        package["version"]
            .as_str()
            .context("Missing locked version")?
            .to_owned(),
        package["checksum"]
            .as_str()
            .context("Missing registry checksum")?
            .to_owned(),
    ))
}

fn fetch_from_git(url: &str, reference: Option<&str>, cache_dir: &Path) -> Result<FetchedCrate> {
    let cache_dir = prepare_cache_dir(cache_dir)?;
    let checkout = TempDir::new_in(cache_dir)?;
    let path = checkout.path().join("repo");
    // Never reuse a checkout or its Git config, even if it has the expected origin.
    // A fresh clone also isolates different revisions requested in the same build.
    let output = Command::new("git")
        .args(["clone", "--no-local", "--no-checkout", "--", url])
        .arg(&path)
        .output()
        .context("Failed to run git clone")?;
    anyhow::ensure!(
        output.status.success(),
        "Failed to clone {url}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let revision = if let Some(reference) = reference {
        anyhow::ensure!(!reference.starts_with('-'), "Invalid Git reference");
        let output = Command::new("git")
            .args(["fetch", "--", "origin", reference])
            .current_dir(&path)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Failed to fetch Git reference {reference}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        "FETCH_HEAD"
    } else {
        "HEAD"
    };
    let output = Command::new("git")
        .args(["checkout", "--detach", revision, "--"])
        .current_dir(&path)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "Failed to checkout {revision}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fetched(path, Some(checkout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_sources() {
        assert!(matches!(
            CrateSource::parse("./examples/echo").unwrap(),
            CrateSource::Local(_)
        ));
        assert!(
            matches!(CrateSource::parse("clap@4").unwrap(), CrateSource::CratesIo { name, version: Some(v) } if name == "clap" && v == "4")
        );
        assert!(
            matches!(CrateSource::parse("github:user/repo#v1").unwrap(), CrateSource::Git { url, reference: Some(r) } if url == "https://github.com/user/repo.git" && r == "v1")
        );
        assert!(CrateSource::parse("git:https://example.test/repo#--orphan").is_err());
        assert!(CrateSource::parse("bad\"name").is_err());
    }

    #[test]
    fn registry_selection_uses_resolved_identity_not_prefix_or_first_version() {
        let package = |id: &str, name: &str, path: &str| json!({"id": id, "name": name, "source": "registry+https://example.test", "manifest_path": path});
        let mut metadata = json!({
            "resolve": {"root": "root", "nodes": [{"id": "root", "deps": [{"name": "rustbb_source", "pkg": "new"}]}]},
            "packages": [package("other", "tool-extra", "/cache/tool-extra-9/Cargo.toml"), package("old", "tool", "/cache/tool-1/Cargo.toml"), package("new", "tool", "/cache/tool-2/Cargo.toml")]
        });
        assert_eq!(
            resolved_registry_path(&metadata, "tool").unwrap().unwrap(),
            Path::new("/cache/tool-2")
        );
        assert!(resolved_registry_path(&metadata, "tool-extra").is_err());
        metadata["resolve"]["nodes"][0]["deps"] = json!([]);
        assert!(resolved_registry_path(&metadata, "tool").unwrap().is_none());
    }

    #[test]
    fn binary_only_lock_selection_preserves_version_source_and_checksum() {
        let mut lock: toml::Value = toml::from_str(
            r#"
[[package]]
name = "rustbb-source-resolver"
version = "0.0.0"
dependencies = ["tool 2.0.0 (registry+https://expected.test)"]
[[package]]
name = "tool"
version = "1.0.0"
source = "registry+https://expected.test"
checksum = "old"
[[package]]
name = "tool"
version = "2.0.0"
source = "registry+https://expected.test"
checksum = "selected"
[[package]]
name = "tool"
version = "2.0.0"
source = "registry+https://other.test"
checksum = "wrong-registry"
"#,
        )
        .unwrap();
        assert_eq!(
            locked_registry_package(&lock, "tool").unwrap(),
            ("2.0.0".into(), "selected".into())
        );
        assert!(locked_registry_package(&lock, "tool-extra").is_err());
        lock["package"][2]["source"] = toml::Value::String("git+https://expected.test".into());
        assert!(locked_registry_package(&lock, "tool").is_err());
    }

    #[test]
    fn git_requests_are_fresh_isolated_and_support_commit_ids() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("source");
        fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(["-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false"])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "--quiet"]);
        let write = |version: &str| {
            fs::write(
                repo.join("Cargo.toml"),
                format!("[package]\nname='tool'\nversion='{version}'\nedition='2021'\n"),
            )
            .unwrap();
            fs::create_dir_all(repo.join("src")).unwrap();
            fs::write(repo.join("src/main.rs"), "fn main(){}").unwrap();
            git(&["add", "."]);
            git(&[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                version,
            ]);
        };
        write("1.0.0");
        let first = git(&["rev-parse", "HEAD"]);
        git(&["tag", "v1"]);
        write("2.0.0");
        let cache = temp.path().join("cache");
        let url = repo.to_str().unwrap();
        let old = fetch_from_git(url, Some(&first), &cache).unwrap();
        let tag = fetch_from_git(url, Some("v1"), &cache).unwrap();
        let current = fetch_from_git(url, None, &cache).unwrap();
        assert_ne!(old.path, current.path);
        assert!(fs::read_to_string(old.path.join("Cargo.toml"))
            .unwrap()
            .contains("1.0.0"));
        assert!(fs::read_to_string(tag.path.join("Cargo.toml"))
            .unwrap()
            .contains("1.0.0"));
        assert!(fs::read_to_string(current.path.join("Cargo.toml"))
            .unwrap()
            .contains("2.0.0"));
        assert!(fetch_from_git(url, Some("does-not-exist"), &cache).is_err());
        let old_path = old.path.clone();
        drop(old);
        assert!(!old_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cache_rejects_symlinks_and_shared_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = TempDir::new().unwrap();
        let cache = temp.path().join("cache");
        prepare_cache_dir(&cache).unwrap();
        assert_eq!(
            fs::metadata(&cache).unwrap().permissions().mode() & 0o777,
            0o700
        );
        symlink(&cache, temp.path().join("link")).unwrap();
        assert!(prepare_cache_dir(&temp.path().join("link")).is_err());
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(prepare_cache_dir(&cache).is_err());
    }
}
