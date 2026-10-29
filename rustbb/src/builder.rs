use anyhow::{Context, Result};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use syn::{Item, Lit, Meta};
use tempfile::TempDir;

use crate::{codegen, discovery, transform};

pub fn build(
    crate_paths: &[PathBuf],
    output_name: &str,
    release: bool,
    enabled_features: &[String],
    no_default_features: bool,
) -> Result<()> {
    println!("Analyzing crates...");
    let infos = crate_paths
        .iter()
        .map(|p| discovery::analyze_crate(p))
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(!infos.is_empty(), "No commands requested");
    let mut names = HashSet::new();
    for info in &infos {
        anyhow::ensure!(
            names.insert(&info.name),
            "Duplicate command name: {}",
            info.name
        );
    }
    let runtime = find_runtime_path()?;
    let generated = TempDir::new()?;
    for (index, info) in infos.iter().enumerate() {
        let dest = generated.path().join(format!("commands/{index}"));
        copy_package(&info.path, &dest)?;
        let sources = collect_sources(&info.path, &info.main_path)?;
        let mut parsers = transform::ParserIndex::default();
        for source in &sources {
            transform::collect_parsers(&source.text, &source.module, &mut parsers)?;
        }
        for source in sources {
            let transformed = transform::transform_source(
                &source.text,
                &source.module,
                &parsers,
                source.path == info.main_path,
            )
            .with_context(|| format!("Cannot transform {}", source.path.display()))?;
            fs::write(
                dest.join(source.path.strip_prefix(&info.path)?),
                transformed,
            )?;
        }
        let manifest = codegen::command_manifest(
            info,
            index,
            &runtime,
            enabled_features,
            no_default_features,
        )?;
        prepare_build_script(info, &dest)?;
        fs::write(dest.join("Cargo.toml"), toml::to_string_pretty(&manifest)?)?;
        println!("  ✓ {}", info.name);
    }
    let manifest =
        codegen::combined_manifest(&infos, &runtime, enabled_features, no_default_features)?;
    fs::write(
        generated.path().join("Cargo.toml"),
        toml::to_string_pretty(&manifest)?,
    )?;
    fs::create_dir(generated.path().join("src"))?;
    fs::write(
        generated.path().join("src/main.rs"),
        codegen::generate_main_rs(&infos),
    )?;
    crate::util::copy_cargo_config(generated.path())?;

    println!("Building{}...", if release { " (release)" } else { "" });
    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "--message-format=json",
        "--package",
        "rustbb-combined",
        "--bin",
        "rustbb_combined",
    ]);
    if release {
        cmd.arg("--release");
    }
    let output = cmd
        .current_dir(generated.path())
        .output()
        .context("Failed to run cargo build")?;
    let messages: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if !output.status.success() {
        for message in &messages {
            if let Some(rendered) = message["message"]["rendered"].as_str() {
                eprint!("{rendered}");
            }
        }
        anyhow::bail!(
            "Cargo build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let executable = messages
        .iter()
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == "rustbb_combined")
        .find_map(|m| m["executable"].as_str())
        .context("Cargo did not report the combined executable")?;
    fs::copy(executable, output_name)
        .with_context(|| format!("Cannot copy binary to {output_name}"))?;
    println!("✓ Built: {output_name}");
    println!(
        "  Commands: {:?}",
        infos.iter().map(|i| &i.name).collect::<Vec<_>>()
    );
    Ok(())
}

struct SourceFile {
    path: PathBuf,
    module: Vec<String>,
    text: String,
}

/// Supply binary-only Cargo metadata to the wrapper's compiler, including
/// macro expansions and OUT_DIR includes. Inject into the existing build main
/// without changing its signature, control flow, module paths or return value.
fn prepare_build_script(info: &discovery::CrateInfo, dest: &Path) -> Result<()> {
    let path = dest.join(codegen::build_script_path(info)?);
    let text = if info.build_script.is_some() {
        fs::read_to_string(&path)?
    } else {
        anyhow::ensure!(
            !path.exists(),
            "Input uses reserved rustbb_build_metadata.rs path"
        );
        "fn main() {}".to_owned()
    };
    let mut script = syn::parse_file(&text).context("Cannot parse original build script")?;
    let binary_name = &info.binary_name;
    let mut found = false;
    for item in &mut script.items {
        if let Item::Fn(function) = item {
            if function.sig.ident == "main" {
                function.block.stmts.insert(
                    0,
                    syn::parse_quote! {
                        ::std::println!("cargo:rustc-env=CARGO_BIN_NAME={}", #binary_name);
                    },
                );
                found = true;
            }
        }
    }
    anyhow::ensure!(
        found,
        "Build script has no explicit main function; cannot supply binary metadata"
    );
    fs::write(path, prettyplease::unparse(&script))?;
    Ok(())
}

/// Consider every literal path that cfg_attr can select. Predicates may come
/// from build scripts or cross-compilation, so host-only cfg evaluation is unsafe.
fn module_paths(attrs: &[syn::Attribute]) -> Result<(Vec<PathBuf>, bool)> {
    fn collect(
        meta: &Meta,
        conditional: bool,
        paths: &mut Vec<PathBuf>,
        explicit: &mut bool,
    ) -> Result<()> {
        match meta {
            Meta::NameValue(value) if value.path.is_ident("path") => {
                let syn::Expr::Lit(literal) = &value.value else {
                    anyhow::bail!("Module path must be a string literal");
                };
                let Lit::Str(path) = &literal.lit else {
                    anyhow::bail!("Module path must be a string literal");
                };
                paths.push(PathBuf::from(path.value()));
                *explicit |= !conditional;
            }
            Meta::List(list) if list.path.is_ident("cfg_attr") => {
                let values = list.parse_args_with(
                    syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
                )?;
                for value in values.iter().skip(1) {
                    collect(value, true, paths, explicit)?;
                }
            }
            _ => (),
        }
        Ok(())
    }
    let mut paths = Vec::new();
    let mut explicit = false;
    for attr in attrs {
        collect(&attr.meta, false, &mut paths, &mut explicit)?;
    }
    Ok((paths, explicit))
}

/// Walk the actual module tree, including #[path] and inline modules. Every
/// reachable command module gets the same AST transformation as the entry file.
fn collect_sources(root: &Path, entry: &Path) -> Result<Vec<SourceFile>> {
    fn visit_items(
        root: &Path,
        items: &[Item],
        dir: &Path,
        module: &[String],
        sources: &mut Vec<SourceFile>,
        active: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        for item in items {
            if let Item::Mod(m) = item {
                let mut path = module.to_vec();
                path.push(m.ident.to_string());
                let (locations, explicit) = module_paths(&m.attrs)?;
                let conditional = m
                    .attrs
                    .iter()
                    .any(|a| a.path().is_ident("cfg") || a.path().is_ident("cfg_attr"));
                let mut candidates: Vec<_> = locations.iter().map(|p| dir.join(p)).collect();
                if let Some((_, items)) = &m.content {
                    if !explicit {
                        candidates.push(dir.join(m.ident.to_string()));
                    }
                    candidates.sort();
                    candidates.dedup();
                    for candidate in candidates {
                        if conditional && !candidate.exists() {
                            continue;
                        }
                        visit_items(root, items, &candidate, &path, sources, active)?;
                    }
                } else {
                    if !explicit {
                        let flat = dir.join(format!("{}.rs", m.ident));
                        candidates.push(if flat.exists() {
                            flat
                        } else {
                            dir.join(m.ident.to_string()).join("mod.rs")
                        });
                    }
                    candidates.sort();
                    candidates.dedup();
                    for file in candidates {
                        if conditional && !file.exists() {
                            // All literal conditional paths are considered. Cargo
                            // will reject a missing file if that branch is active.
                            continue;
                        }
                        let file = file
                            .canonicalize()
                            .with_context(|| format!("Cannot locate module {}", file.display()))?;
                        let children = if file.file_name().is_some_and(|n| n == "mod.rs") {
                            file.parent().unwrap().to_path_buf()
                        } else {
                            file.with_extension("")
                        };
                        visit_file(root, &file, &children, &path, sources, active)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn visit_file(
        root: &Path,
        file: &Path,
        dir: &Path,
        module: &[String],
        sources: &mut Vec<SourceFile>,
        active: &mut HashSet<PathBuf>,
    ) -> Result<()> {
        anyhow::ensure!(file.starts_with(root), "External source module {} is outside the package; move it into the package before combining", file.display());
        anyhow::ensure!(
            active.insert(file.to_path_buf()),
            "Cyclic source module: {}",
            file.display()
        );
        let text = fs::read_to_string(file)?;
        let parsed =
            syn::parse_file(&text).with_context(|| format!("Cannot parse {}", file.display()))?;
        visit_items(root, &parsed.items, dir, module, sources, active)?;
        sources.push(SourceFile {
            path: file.to_path_buf(),
            module: module.to_vec(),
            text,
        });
        active.remove(file);
        Ok(())
    }
    let mut sources = Vec::new();
    visit_file(
        root,
        entry,
        entry.parent().context("Invalid entry source")?,
        &[],
        &mut sources,
        &mut HashSet::new(),
    )?;
    Ok(sources)
}

fn copy_package(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" || name == ".git" || name == ".cargo" {
            continue;
        }
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_package(&entry.path(), &dst.join(&name))?;
        } else if ty.is_file() {
            fs::copy(entry.path(), dst.join(&name))?;
        } else if ty.is_symlink() {
            anyhow::bail!(
                "Symlink in package source is not supported: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn find_runtime_path() -> Result<PathBuf> {
    let mut candidates = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../rustbb_runtime")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("rustbb_runtime"));
        }
    }
    candidates.extend([
        PathBuf::from("rustbb_runtime"),
        PathBuf::from("../rustbb_runtime"),
    ]);
    candidates
        .into_iter()
        .find(|p| p.join("Cargo.toml").is_file())
        .context("Cannot locate rustbb_runtime alongside rustbb or in its workspace")?
        .canonicalize()
        .map_err(Into::into)
}
