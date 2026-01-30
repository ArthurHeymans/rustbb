use anyhow::Result;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use crate::discovery::{CrateInfo, DepInfo};
use crate::transform::sanitize_name;

/// Generated combined crate source files
pub struct GeneratedCrate {
    pub cargo_toml: String,
    pub main_rs: String,
    /// Simple command modules (single file, no internal modules)
    pub command_modules: HashMap<String, String>,
    /// Crates that need their entire source directory copied
    /// Maps sanitized crate name to (src_dir, transformed_main_content)
    pub crates_with_modules: HashMap<String, (PathBuf, String)>,
}

pub fn generate_combined_crate(
    crates: &[CrateInfo],
    output_name: &str,
    transformed_sources: &HashMap<String, String>,
    runtime_path: &str,
) -> Result<GeneratedCrate> {
    // Generate Cargo.toml
    let cargo_toml = generate_cargo_toml(crates, output_name, runtime_path)?;

    // Generate main.rs
    let main_rs = generate_main_rs(crates)?;

    // Separate crates into simple modules and those with internal modules
    let mut command_modules: HashMap<String, String> = HashMap::new();
    let mut crates_with_modules: HashMap<String, (PathBuf, String)> = HashMap::new();

    for c in crates {
        if let Some(src) = transformed_sources.get(&c.name) {
            let sanitized = sanitize_name(&c.name);
            if c.has_internal_modules {
                // This crate needs its source directory copied
                crates_with_modules.insert(sanitized, (c.src_dir.clone(), src.clone()));
            } else {
                // Simple single-file module
                command_modules.insert(c.name.clone(), src.clone());
            }
        }
    }

    Ok(GeneratedCrate {
        cargo_toml,
        main_rs,
        command_modules,
        crates_with_modules,
    })
}

fn generate_cargo_toml(
    crates: &[CrateInfo],
    output_name: &str,
    runtime_path: &str,
) -> Result<String> {
    // Merge dependencies from all crates
    let merged_deps = merge_dependencies(crates);

    let mut deps_str = String::new();
    for (name, info) in &merged_deps {
        let dep_spec = format_dependency(name, info);
        deps_str.push_str(&dep_spec);
        deps_str.push('\n');
    }

    let toml = format!(
        r#"[package]
name = "{output_name}"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "{output_name}"
path = "src/main.rs"

[dependencies]
rustbb_runtime = {{ path = "{runtime_path}" }}
{deps_str}
[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true
"#
    );

    Ok(toml)
}

/// Merge dependencies from multiple crates
fn merge_dependencies(crates: &[CrateInfo]) -> BTreeMap<String, DepInfo> {
    let mut merged: BTreeMap<String, DepInfo> = BTreeMap::new();

    for crate_info in crates {
        // If this crate has a library, add the crate itself as a dependency
        // so that `use cratename::...` imports work
        if crate_info.has_library {
            merged.insert(
                crate_info.name.clone(),
                DepInfo {
                    version: crate_info.version.clone(),
                    features: vec![],
                    optional: false,
                },
            );
        }

        for (name, info) in &crate_info.dependencies {
            if let Some(existing) = merged.get_mut(name) {
                // Merge features
                let mut features: HashSet<String> = existing.features.iter().cloned().collect();
                features.extend(info.features.iter().cloned());
                existing.features = features.into_iter().collect();
                existing.features.sort();

                // Use the most specific version (prefer explicit over None)
                if existing.version.is_none() && info.version.is_some() {
                    existing.version = info.version.clone();
                }
                // If both have versions, we'd ideally resolve them, but for now keep the first
            } else {
                merged.insert(name.clone(), info.clone());
            }
        }
    }

    merged
}

/// Format a dependency for Cargo.toml
fn format_dependency(name: &str, info: &DepInfo) -> String {
    let has_features = !info.features.is_empty();
    let version = info.version.as_deref().unwrap_or("*");

    if has_features {
        let features_str = info
            .features
            .iter()
            .map(|f| format!("\"{}\"", f))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} = {{ version = \"{}\", features = [{}] }}",
            name, version, features_str
        )
    } else {
        format!("{} = \"{}\"", name, version)
    }
}

fn generate_main_rs(crates: &[CrateInfo]) -> Result<String> {
    let cmd_names: Vec<&str> = crates.iter().map(|c| c.name.as_str()).collect();

    // Generate module declarations
    let mod_decls: String = cmd_names
        .iter()
        .map(|name| format!("mod {};", sanitize_name(name)))
        .collect::<Vec<_>>()
        .join("\n");

    // Generate registration
    let registrations: String = cmd_names
        .iter()
        .map(|name| {
            let sanitized = sanitize_name(name);
            format!(
                "    registry.register(\"{}\", {}::rustbb_cmd_{});",
                name, sanitized, sanitized
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let main_rs = format!(
        r#"use rustbb_runtime::{{Registry, dispatch}};

{mod_decls}

fn main() {{
    let mut registry = Registry::new();

{registrations}

    dispatch(&registry);
}}
"#
    );

    Ok(main_rs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::TransformStrategy;
    use std::path::PathBuf;

    #[test]
    fn test_generate_main_rs() {
        let crates = vec![
            CrateInfo {
                name: "echo".to_string(),
                path: PathBuf::from("/tmp/echo"),
                main_path: PathBuf::from("/tmp/echo/src/main.rs"),
                strategy: TransformStrategy::SimpleMain,
                dependencies: BTreeMap::new(),
                has_library: false,
                version: None,
                build_script_outputs: BTreeMap::new(),
                has_internal_modules: false,
                src_dir: PathBuf::from("/tmp/echo/src"),
            },
            CrateInfo {
                name: "cat".to_string(),
                path: PathBuf::from("/tmp/cat"),
                main_path: PathBuf::from("/tmp/cat/src/main.rs"),
                strategy: TransformStrategy::SimpleMain,
                dependencies: BTreeMap::new(),
                has_library: false,
                version: None,
                build_script_outputs: BTreeMap::new(),
                has_internal_modules: false,
                src_dir: PathBuf::from("/tmp/cat/src"),
            },
        ];

        let main_rs = generate_main_rs(&crates).unwrap();
        assert!(main_rs.contains("mod echo;"));
        assert!(main_rs.contains("mod cat;"));
        assert!(main_rs.contains("registry.register(\"echo\""));
        assert!(main_rs.contains("registry.register(\"cat\""));
    }

    #[test]
    fn test_merge_dependencies() {
        let crates = vec![
            CrateInfo {
                name: "cmd1".to_string(),
                path: PathBuf::from("/tmp/cmd1"),
                main_path: PathBuf::from("/tmp/cmd1/src/main.rs"),
                strategy: TransformStrategy::SimpleMain,
                dependencies: {
                    let mut deps = BTreeMap::new();
                    deps.insert(
                        "clap".to_string(),
                        DepInfo {
                            version: Some("4".to_string()),
                            features: vec!["derive".to_string()],
                            optional: false,
                        },
                    );
                    deps
                },
                has_library: false,
                version: None,
                build_script_outputs: BTreeMap::new(),
                has_internal_modules: false,
                src_dir: PathBuf::from("/tmp/cmd1/src"),
            },
            CrateInfo {
                name: "cmd2".to_string(),
                path: PathBuf::from("/tmp/cmd2"),
                main_path: PathBuf::from("/tmp/cmd2/src/main.rs"),
                strategy: TransformStrategy::SimpleMain,
                dependencies: {
                    let mut deps = BTreeMap::new();
                    deps.insert(
                        "clap".to_string(),
                        DepInfo {
                            version: Some("4".to_string()),
                            features: vec!["derive".to_string(), "env".to_string()],
                            optional: false,
                        },
                    );
                    deps
                },
                has_library: false,
                version: None,
                build_script_outputs: BTreeMap::new(),
                has_internal_modules: false,
                src_dir: PathBuf::from("/tmp/cmd2/src"),
            },
        ];

        let merged = merge_dependencies(&crates);
        let clap = merged.get("clap").unwrap();
        assert!(clap.features.contains(&"derive".to_string()));
        assert!(clap.features.contains(&"env".to_string()));
    }

    #[test]
    fn test_format_dependency() {
        let info = DepInfo {
            version: Some("4".to_string()),
            features: vec!["derive".to_string()],
            optional: false,
        };
        let formatted = format_dependency("clap", &info);
        assert!(formatted.contains("clap"));
        assert!(formatted.contains("4"));
        assert!(formatted.contains("derive"));
    }
}
