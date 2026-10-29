//! Each command is a separate Cargo library. Its cfg/features, aliases and
//! dependency sources remain local instead of being flattened into one manifest.

use anyhow::{Context, Result};
use std::path::Path;
use toml::{map::Map, Value};

use crate::discovery::CrateInfo;

fn command_package_name(info: &CrateInfo, index: usize) -> String {
    // Avoid Cargo lockfile identity collisions with a command's original library.
    // Binary-only packages can retain their compile-time package metadata.
    if info.library_name.is_some() {
        format!("{}-rustbb-wrapper-{index}", info.name)
    } else {
        info.name.clone()
    }
}

pub fn command_manifest(
    info: &CrateInfo,
    index: usize,
    runtime_path: &Path,
    features: &[String],
    no_default_features: bool,
) -> Result<Value> {
    let mut doc = info.manifest.clone();
    let root = doc.as_table_mut().context("Invalid package manifest")?;
    for key in [
        "workspace",
        "bin",
        "example",
        "test",
        "bench",
        "lib",
        "profile",
        "patch",
        "replace",
    ] {
        root.remove(key);
    }
    let package = root
        .get_mut("package")
        .and_then(Value::as_table_mut)
        .context("Missing package")?;
    for (key, value) in package.iter_mut() {
        if inherited(value) {
            *value = info
                .workspace_manifest
                .get("workspace")
                .and_then(|w| w.get("package"))
                .and_then(|p| p.get(key))
                .with_context(|| format!("Cannot resolve workspace package.{key}"))?
                .clone();
            if matches!(key.as_str(), "readme" | "license-file") {
                absolute_path(value, &info.workspace_root)?;
            }
        }
    }
    package.remove("workspace");
    package.remove("default-run");
    if info.library_name.is_some() {
        // The original library alone owns its native-link identity. The copied
        // build script still runs for binary-specific generated files and cfgs.
        package.remove("links");
    }
    package.insert(
        "build".into(),
        Value::String(build_script_path(info)?.to_string_lossy().into_owned()),
    );
    package.insert(
        "name".into(),
        Value::String(command_package_name(info, index)),
    );
    package.insert("publish".into(), Value::Boolean(false));
    for key in ["autobins", "autoexamples", "autotests", "autobenches"] {
        package.insert(key.into(), Value::Boolean(false));
    }
    root.insert(
        "lib".into(),
        Value::Table(Map::from_iter([
            (
                "name".into(),
                Value::String(format!("rustbb_command_{index}")),
            ),
            (
                "path".into(),
                Value::String(
                    info.main_path
                        .strip_prefix(&info.path)?
                        .to_string_lossy()
                        .into_owned(),
                ),
            ),
        ])),
    );
    resolve_dependency_sections(root, info)?;
    if root.get("lints").is_some_and(inherited) {
        root.insert(
            "lints".into(),
            info.workspace_manifest
                .get("workspace")
                .and_then(|w| w.get("lints"))
                .context("Cannot resolve workspace lints")?
                .clone(),
        );
    }
    let deps = root
        .entry("dependencies")
        .or_insert_with(|| Value::Table(Map::new()))
        .as_table_mut()
        .context("Invalid dependencies")?;
    anyhow::ensure!(
        !deps.contains_key("rustbb_runtime"),
        "Input package already depends on rustbb_runtime"
    );
    deps.insert("rustbb_runtime".into(), path_dependency(runtime_path));
    if let Some(library_name) = &info.library_name {
        // Keep the original library in its original workspace, with its original
        // manifest and transitive path dependencies. Never rewrite its sources.
        anyhow::ensure!(
            !deps.contains_key(library_name),
            "Library dependency name collision: {library_name}"
        );
        let mut dep = path_dependency(&info.path);
        let table = dep.as_table_mut().unwrap();
        table.insert("package".into(), Value::String(info.name.clone()));
        table.insert(
            "default-features".into(),
            Value::Boolean(!no_default_features),
        );
        table.insert(
            "features".into(),
            Value::Array(features.iter().cloned().map(Value::String).collect()),
        );
        deps.insert(library_name.clone(), dep);
    }
    Ok(doc)
}

pub fn build_script_path(info: &CrateInfo) -> Result<std::path::PathBuf> {
    match &info.build_script {
        Some(path) => Ok(path
            .strip_prefix(&info.path)
            .context("Build script is outside the package")?
            .to_path_buf()),
        None => Ok(std::path::PathBuf::from("rustbb_build_metadata.rs")),
    }
}

fn inherited(value: &Value) -> bool {
    value.get("workspace").and_then(Value::as_bool) == Some(true)
}

fn absolute_path(value: &mut Value, base: &Path) -> Result<()> {
    if let Some(path) = value.as_str() {
        *value = Value::String(
            base.join(path)
                .to_str()
                .context("Non-UTF-8 dependency path")?
                .to_owned(),
        );
    }
    Ok(())
}

fn resolve_dependency_sections(table: &mut Map<String, Value>, info: &CrateInfo) -> Result<()> {
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = table.get_mut(section).and_then(Value::as_table_mut) {
            for (name, dep) in deps {
                let base = if inherited(dep) {
                    let ws_dep = info
                        .workspace_manifest
                        .get("workspace")
                        .and_then(|w| w.get("dependencies"))
                        .and_then(|d| d.get(name))
                        .with_context(|| format!("Cannot resolve workspace dependency {name}"))?;
                    let mut merged = match ws_dep {
                        Value::String(version) => {
                            Map::from_iter([("version".into(), Value::String(version.clone()))])
                        }
                        Value::Table(table) => table.clone(),
                        _ => anyhow::bail!("Invalid workspace dependency {name}"),
                    };
                    for (key, value) in dep.as_table().unwrap() {
                        match key.as_str() {
                            "workspace" => (),
                            "features" => {
                                let features = merged
                                    .entry("features")
                                    .or_insert_with(|| Value::Array(Vec::new()))
                                    .as_array_mut()
                                    .context("Invalid workspace dependency features")?;
                                features.extend(
                                    value
                                        .as_array()
                                        .context("Invalid dependency features")?
                                        .iter()
                                        .cloned(),
                                );
                            }
                            "default-features"
                                if value.as_bool() == Some(false)
                                    && merged.get("default-features").and_then(Value::as_bool)
                                        != Some(false) => {}
                            _ => {
                                merged.insert(key.clone(), value.clone());
                            }
                        }
                    }
                    *dep = Value::Table(merged);
                    &info.workspace_root
                } else {
                    &info.path
                };
                if let Some(path) = dep.get_mut("path") {
                    absolute_path(path, base)?;
                }
            }
        }
    }
    if let Some(targets) = table.get_mut("target").and_then(Value::as_table_mut) {
        for (_, target) in targets.iter_mut() {
            resolve_dependency_sections(
                target.as_table_mut().context("Invalid target table")?,
                info,
            )?;
        }
    }
    Ok(())
}

fn path_dependency(path: &Path) -> Value {
    Value::Table(Map::from_iter([(
        "path".into(),
        Value::String(path.to_string_lossy().into_owned()),
    )]))
}

pub fn combined_manifest(
    crates: &[CrateInfo],
    runtime_path: &Path,
    features: &[String],
    no_default_features: bool,
) -> Result<Value> {
    let mut doc = toml::toml! {
        [package]
        name = "rustbb-combined"
        version = "0.1.0"
        edition = "2021"
        [[bin]]
        name = "rustbb_combined"
        path = "src/main.rs"
        [workspace]
        resolver = "2"
        [profile.release]
        opt-level = "z"
        lto = true
        codegen-units = 1
        panic = "abort"
        strip = true
    };
    let mut deps = Map::new();
    deps.insert("rustbb_runtime".into(), path_dependency(runtime_path));
    for (index, info) in crates.iter().enumerate() {
        let mut dep = path_dependency(Path::new(&format!("commands/{index}")));
        let table = dep.as_table_mut().unwrap();
        table.insert(
            "package".into(),
            Value::String(command_package_name(info, index)),
        );
        table.insert(
            "default-features".into(),
            Value::Boolean(!no_default_features),
        );
        table.insert(
            "features".into(),
            Value::Array(features.iter().cloned().map(Value::String).collect()),
        );
        deps.insert(format!("rustbb_command_{index}"), dep);
    }
    doc.insert("dependencies".into(), Value::Table(deps));
    // Cargo only honors patches/replacements at the workspace root. Preserve
    // source identity; conflicting policies must fail rather than select one.
    for info in crates {
        if let Some(replacements) = info
            .workspace_manifest
            .get("replace")
            .and_then(Value::as_table)
        {
            let entries = doc
                .entry("replace")
                .or_insert_with(|| Value::Table(Map::new()))
                .as_table_mut()
                .unwrap();
            for (name, spec) in replacements {
                let mut spec = spec.clone();
                if let Some(path) = spec.get_mut("path") {
                    absolute_path(path, &info.workspace_root)?;
                }
                if let Some(existing) = entries.insert(name.clone(), spec.clone()) {
                    anyhow::ensure!(
                        existing == spec,
                        "Conflicting workspace replacements for {name}"
                    );
                }
            }
        }
        if let Some(patches) = info
            .workspace_manifest
            .get("patch")
            .and_then(Value::as_table)
        {
            for (source, entries) in patches {
                for (name, spec) in entries.as_table().context("Invalid patch table")? {
                    let mut spec = spec.clone();
                    if let Some(path) = spec.get_mut("path") {
                        absolute_path(path, &info.workspace_root)?;
                    }
                    let patch = doc
                        .entry("patch")
                        .or_insert_with(|| Value::Table(Map::new()))
                        .as_table_mut()
                        .unwrap();
                    let entries = patch
                        .entry(source)
                        .or_insert_with(|| Value::Table(Map::new()))
                        .as_table_mut()
                        .unwrap();
                    if let Some(existing) = entries.insert(name.clone(), spec.clone()) {
                        anyhow::ensure!(
                            existing == spec,
                            "Conflicting workspace patches for {name}"
                        );
                    }
                }
            }
        }
    }
    Ok(Value::Table(doc))
}

pub fn generate_main_rs(crates: &[CrateInfo]) -> String {
    let registrations = crates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "    registry.register({:?}, rustbb_command_{i}::rustbb_entry);",
                c.name
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("fn main() -> std::process::ExitCode {{\n    let mut registry = rustbb_runtime::Registry::new();\n{registrations}\n    rustbb_runtime::dispatch(&registry)\n}}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn info(manifest: &str, workspace: &str) -> CrateInfo {
        CrateInfo {
            name: "app".into(),
            path: PathBuf::from("/ws/app"),
            main_path: PathBuf::from("/ws/app/src/main.rs"),
            binary_name: "app".into(),
            build_script: None,
            library_name: None,
            manifest: toml::from_str(manifest).unwrap(),
            workspace_root: PathBuf::from("/ws"),
            workspace_manifest: toml::from_str(workspace).unwrap(),
        }
    }

    #[test]
    fn preserves_complete_workspace_dependency_and_target_tables() {
        let info = info(
            r#"
[package]
name = "app"
version.workspace = true
edition.workspace = true
[dependencies]
policy = { workspace = true, features = ["local"], optional = true }
[target.'cfg(target_os = "some.os")'.dependencies]
helper = { path = "../helper", default-features = false, features = ["required"] }
"#,
            r#"
[workspace.package]
version = "1.2.3"
edition = "2024"
[workspace.dependencies]
policy = { git = "https://example.test/policy", rev = "abc123", package = "actual-policy", default-features = false, features = ["base"] }
"#,
        );
        let doc = command_manifest(&info, 0, Path::new("/runtime"), &[], false).unwrap();
        assert_eq!(doc["package"]["version"].as_str(), Some("1.2.3"));
        assert_eq!(doc["package"]["edition"].as_str(), Some("2024"));
        let dep = &doc["dependencies"]["policy"];
        assert_eq!(dep["git"].as_str(), Some("https://example.test/policy"));
        assert_eq!(dep["rev"].as_str(), Some("abc123"));
        assert_eq!(dep["package"].as_str(), Some("actual-policy"));
        assert_eq!(dep["optional"].as_bool(), Some(true));
        assert_eq!(dep["default-features"].as_bool(), Some(false));
        assert_eq!(dep["features"].as_array().unwrap().len(), 2);
        let helper = &doc["target"]["cfg(target_os = \"some.os\")"]["dependencies"]["helper"];
        assert_eq!(helper["path"].as_str(), Some("/ws/app/../helper"));
        assert_eq!(helper["default-features"].as_bool(), Some(false));
        assert_eq!(helper["features"][0].as_str(), Some("required"));
    }

    #[test]
    fn retains_root_source_overrides_and_rejects_conflicts() {
        let first = info("[package]\nname='app'\nversion='0.1.0'\ndefault-run='app'", "[patch.crates-io]\npolicy={path='vendor/policy'}\n[replace]\n'old:1.0.0'={git='https://example.test/old',rev='abcdef'}");
        let doc = combined_manifest(
            std::slice::from_ref(&first),
            Path::new("/runtime"),
            &[],
            false,
        )
        .unwrap();
        assert_eq!(
            doc["patch"]["crates-io"]["policy"]["path"].as_str(),
            Some("/ws/vendor/policy")
        );
        assert_eq!(doc["replace"]["old:1.0.0"]["rev"].as_str(), Some("abcdef"));
        let wrapper = command_manifest(&first, 0, Path::new("/runtime"), &[], false).unwrap();
        assert!(wrapper["package"].get("default-run").is_none());
        let second = info(
            "[package]\nname='other'\nversion='0.1.0'",
            "[patch.crates-io]\npolicy={git='https://example.test/other'}",
        );
        assert!(combined_manifest(&[first, second], Path::new("/runtime"), &[], false).is_err());
    }

    #[test]
    fn missing_workspace_dependency_fails_closed() {
        let info = info(
            "[package]\nname='app'\nversion='0.1.0'\n[dependencies]\npolicy.workspace=true",
            "[workspace]",
        );
        assert!(command_manifest(&info, 0, Path::new("/runtime"), &[], false).is_err());
    }
}
