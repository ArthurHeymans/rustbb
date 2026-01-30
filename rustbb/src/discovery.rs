use anyhow::{Context, Result};
use cargo_toml::{Dependency, Manifest};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use syn::{parse_file, Item};

#[derive(Debug, Clone)]
pub struct CrateInfo {
    pub name: String,
    pub path: PathBuf,
    pub main_path: PathBuf,
    pub strategy: TransformStrategy,
    pub dependencies: BTreeMap<String, DepInfo>,
    /// If true, this crate has a library (lib.rs) that the binary may depend on
    pub has_library: bool,
    /// Version of the crate (for adding as dependency)
    pub version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DepInfo {
    pub version: Option<String>,
    pub features: Vec<String>,
    #[allow(dead_code)]
    pub optional: bool,
}

#[derive(Debug, Clone)]
pub enum TransformStrategy {
    /// Simple fn main() - can be extracted directly
    SimpleMain,
    /// Has #[...] attributes on main - needs special handling
    AttributedMain { attrs: Vec<String> },
    /// Has a library interface we can use
    #[allow(dead_code)]
    LibraryInterface { entry_fn: String },
    /// Cannot be transformed automatically
    Unsupported { reason: String },
}

pub fn analyze_crate(path: &Path) -> Result<CrateInfo> {
    let cargo_toml_path = path.join("Cargo.toml");
    let manifest = Manifest::from_path(&cargo_toml_path).context("Failed to parse Cargo.toml")?;

    let name = manifest
        .package
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "unknown".to_string());

    // Find main.rs
    let main_path = find_main_rs(path, &manifest)?;

    // Analyze main() function
    let source = fs::read_to_string(&main_path).context("Failed to read main.rs")?;
    let strategy = analyze_main_function(&source)?;

    // Collect dependencies with full info, filtering out path-based (workspace) dependencies
    let dependencies: BTreeMap<String, DepInfo> = manifest
        .dependencies
        .iter()
        .filter_map(|(name, dep)| {
            let (actual_name, info) = match dep {
                Dependency::Simple(version) => (
                    name.clone(),
                    Some(DepInfo {
                        version: Some(version.clone()),
                        features: vec![],
                        optional: false,
                    }),
                ),
                Dependency::Detailed(detail) => {
                    // Skip path-based dependencies (workspace members, local crates)
                    if detail.path.is_some() {
                        return None;
                    }
                    // Skip git dependencies for now (they need special handling)
                    if detail.git.is_some() {
                        return None;
                    }
                    // Skip optional dependencies (feature-gated, not needed by default)
                    if detail.optional {
                        return None;
                    }
                    // Use the actual package name if it's different (renamed dependency)
                    let actual_name = detail.package.clone().unwrap_or_else(|| name.clone());
                    (
                        actual_name,
                        Some(DepInfo {
                            version: detail.version.clone(),
                            features: detail.features.clone(),
                            optional: detail.optional,
                        }),
                    )
                }
                Dependency::Inherited(inherited) => {
                    // Inherited dependencies without version info can't be resolved
                    // Skip them unless we have workspace context
                    if inherited.workspace {
                        return None;
                    }
                    // Skip optional dependencies
                    if inherited.optional {
                        return None;
                    }
                    (
                        name.clone(),
                        Some(DepInfo {
                            version: None,
                            features: inherited.features.clone(),
                            optional: inherited.optional,
                        }),
                    )
                }
            };
            info.map(|i| (actual_name, i))
        })
        .collect();

    // Check if this crate has a library component
    let has_library = path.join("src/lib.rs").exists() || manifest.lib.is_some();

    // Get version from manifest
    let version = manifest
        .package
        .as_ref()
        .and_then(|p| p.version.get().ok())
        .map(|v| v.to_string());

    Ok(CrateInfo {
        name,
        path: path.to_path_buf(),
        main_path,
        strategy,
        dependencies,
        has_library,
        version,
    })
}

fn find_main_rs(crate_path: &Path, manifest: &Manifest) -> Result<PathBuf> {
    // First check if there's a [[bin]] entry with an explicit path
    for bin in &manifest.bin {
        if let Some(path) = &bin.path {
            let bin_path = crate_path.join(path);
            if bin_path.exists() {
                return Ok(bin_path);
            }
        }
    }

    // Default candidates
    let candidates = [
        crate_path.join("src/main.rs"),
        crate_path.join("src/bin/main.rs"),
    ];

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    anyhow::bail!("Could not find main.rs in {:?}", crate_path)
}

fn analyze_main_function(source: &str) -> Result<TransformStrategy> {
    // Check for include! macro which indicates build script generated code
    if source.contains("include!(") && source.contains("OUT_DIR") {
        return Ok(TransformStrategy::Unsupported {
            reason: "Uses build script generated code (include! with OUT_DIR)".to_string(),
        });
    }

    let file = parse_file(source).context("Failed to parse Rust source")?;

    for item in &file.items {
        if let Item::Fn(func) = item {
            if func.sig.ident == "main" {
                // Check for attributes (excluding doc comments)
                let attrs: Vec<String> = func
                    .attrs
                    .iter()
                    .filter(|a| !a.path().is_ident("doc"))
                    .map(|a| {
                        // Convert path segments to string
                        a.path()
                            .segments
                            .iter()
                            .map(|s| s.ident.to_string())
                            .collect::<Vec<_>>()
                            .join("::")
                    })
                    .collect();

                if attrs.is_empty() {
                    return Ok(TransformStrategy::SimpleMain);
                } else {
                    return Ok(TransformStrategy::AttributedMain { attrs });
                }
            }
        }
    }

    // No main found
    Ok(TransformStrategy::Unsupported {
        reason: "No main() function found".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analyze_simple_main() {
        let source = r#"
fn main() {
    println!("Hello, world!");
}
"#;
        let strategy = analyze_main_function(source).unwrap();
        assert!(matches!(strategy, TransformStrategy::SimpleMain));
    }

    #[test]
    fn test_analyze_attributed_main() {
        let source = r#"
#[tokio::main]
async fn main() {
    println!("Hello, async world!");
}
"#;
        let strategy = analyze_main_function(source).unwrap();
        assert!(matches!(strategy, TransformStrategy::AttributedMain { .. }));
    }

    #[test]
    fn test_analyze_no_main() {
        let source = r#"
fn not_main() {
    println!("Hello!");
}
"#;
        let strategy = analyze_main_function(source).unwrap();
        assert!(matches!(strategy, TransformStrategy::Unsupported { .. }));
    }
}
