use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

use crate::codegen::{generate_combined_crate, GeneratedCrate};
use crate::discovery::{analyze_crate, run_build_script, CrateInfo, TransformStrategy};
use crate::transform::{
    sanitize_name, transform_main, transform_main_for_module, transform_main_with_build_outputs,
};

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
    let mut build_outputs_map: HashMap<String, HashMap<String, String>> = HashMap::new();

    for info in &crate_infos {
        match &info.strategy {
            TransformStrategy::SimpleMain => {
                let source = fs::read_to_string(&info.main_path)?;
                let transformed_source = if info.has_internal_modules {
                    transform_main_for_module(&source, &info.name)?
                } else {
                    transform_main(&source, &info.name)?
                };
                transformed.insert(info.name.clone(), transformed_source);
                let suffix = if info.has_internal_modules {
                    " (multi-file)"
                } else {
                    ""
                };
                println!("  ✓ {}{}", info.name, suffix);
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
                    let result = if info.has_internal_modules {
                        transform_main_for_module(&source, &info.name)
                    } else {
                        transform_main(&source, &info.name)
                    };
                    match result {
                        Ok(transformed_source) => {
                            transformed.insert(info.name.clone(), transformed_source);
                            let mut suffix = String::new();
                            if is_supported_async {
                                suffix.push_str(" (async");
                            }
                            if info.has_internal_modules {
                                if suffix.is_empty() {
                                    suffix.push_str(" (multi-file");
                                } else {
                                    suffix.push_str(", multi-file");
                                }
                            }
                            if !suffix.is_empty() {
                                suffix.push(')');
                            }
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
            TransformStrategy::BuildScriptMain { attrs } => {
                // Run build script to generate OUT_DIR files
                match run_build_script(&info.path, &info.name) {
                    Ok(build_outputs) => {
                        let source = fs::read_to_string(&info.main_path)?;

                        // Check for async runtime
                        let is_async = attrs
                            .iter()
                            .any(|a| matches!(a.as_str(), "tokio::main" | "async_std::main"));

                        match transform_main_with_build_outputs(&source, &info.name, &build_outputs)
                        {
                            Ok((transformed_source, used_outputs)) => {
                                transformed.insert(info.name.clone(), transformed_source);
                                // Store the build outputs in the crate info for codegen
                                // We'll need to pass these to generate_combined_crate
                                let suffix = if is_async {
                                    " (async, build.rs)"
                                } else {
                                    " (build.rs)"
                                };
                                println!("  ✓ {}{}", info.name, suffix);

                                // Update crate info with build outputs
                                // Note: we need to handle this differently since we're iterating
                                build_outputs_map.insert(info.name.clone(), used_outputs);
                            }
                            Err(e) => {
                                println!("  ✗ {} - transform failed: {}", info.name, e);
                            }
                        }
                    }
                    Err(e) => {
                        println!("  ✗ {} - build script failed: {}", info.name, e);
                    }
                }
            }
            TransformStrategy::UucoreBinMacro { crate_name } => {
                // For uucore::bin! macro, we generate a wrapper that calls the library's uumain
                let wrapper_source = generate_uucore_wrapper(&crate_name, &info.name);
                transformed.insert(info.name.clone(), wrapper_source);
                println!("  ✓ {} (uucore)", info.name);
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

    // Write simple command modules (single file)
    for (name, source) in &generated.command_modules {
        let sanitized = sanitize_name(name);
        fs::write(src_dir.join(format!("{}.rs", sanitized)), source)?;
    }

    // Handle crates with internal modules - copy entire source directory
    for (sanitized_name, (orig_src_dir, transformed_main)) in &generated.crates_with_modules {
        let crate_dir = src_dir.join(sanitized_name);
        fs::create_dir_all(&crate_dir)?;

        // Get exported macros and dependency renames for this crate
        let main_rs = orig_src_dir.join("main.rs");
        let exported_macros = if main_rs.exists() {
            find_exported_macros(&fs::read_to_string(&main_rs).unwrap_or_default())
        } else {
            std::collections::HashSet::new()
        };
        let dep_renames = find_dependency_renames(orig_src_dir);

        // Copy all files from original source directory, transforming crate references
        copy_source_files_recursive(
            orig_src_dir,
            &crate_dir,
            sanitized_name,
            &exported_macros,
            &dep_renames,
        )?;

        // Transform and write the main content as mod.rs
        // This content also needs extern crate renames and other transforms
        let transformed_mod = transform_crate_references(
            transformed_main,
            sanitized_name,
            &exported_macros,
            &dep_renames,
        );
        fs::write(crate_dir.join("mod.rs"), transformed_mod)?;
    }

    // Copy .cargo/config.toml if it exists (for linker settings, etc.)
    copy_cargo_config(base)?;

    Ok(())
}

/// Copy source files recursively, transforming crate references
/// `crate_module_name` is the name of the module (e.g., "lsd") so we can transform
/// `use crate::X` to `use crate::{module}::X`
fn copy_source_files_with_transform(src: &Path, dst: &Path, crate_module_name: &str) -> Result<()> {
    // First, scan the main source file for #[macro_export] macros
    let main_rs = src.join("main.rs");
    let exported_macros = if main_rs.exists() {
        find_exported_macros(&fs::read_to_string(&main_rs).unwrap_or_default())
    } else {
        std::collections::HashSet::new()
    };

    // Also collect renamed extern crates (extern crate X as Y)
    // and package renames from dependencies
    let dep_renames = find_dependency_renames(src);

    copy_source_files_recursive(src, dst, crate_module_name, &exported_macros, &dep_renames)
}

fn copy_source_files_recursive(
    src: &Path,
    dst: &Path,
    crate_module_name: &str,
    exported_macros: &std::collections::HashSet<String>,
    dep_renames: &std::collections::HashMap<String, String>,
) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();

        // Skip main.rs - we'll use our transformed version
        if file_name_str == "main.rs" {
            continue;
        }

        let dst_path = dst.join(&file_name);

        if src_path.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_source_files_recursive(
                &src_path,
                &dst_path,
                crate_module_name,
                exported_macros,
                dep_renames,
            )?;
        } else if file_name_str.ends_with(".rs") {
            // Transform Rust source files
            let content = fs::read_to_string(&src_path)?;
            let transformed = transform_crate_references(
                &content,
                crate_module_name,
                exported_macros,
                dep_renames,
            );
            fs::write(&dst_path, transformed)?;
        } else {
            // Copy non-Rust files as-is
            fs::copy(&src_path, &dst_path)?;
        }
    }

    Ok(())
}

/// Transform grouped imports like `use crate::{A, B, color::{X, Y}};`
/// This handles nested braces correctly
fn transform_grouped_crate_imports(
    source: &str,
    module_name: &str,
    exported_macros: &std::collections::HashSet<String>,
) -> String {
    let mut result = String::new();
    let mut chars = source.char_indices().peekable();
    let pattern = "use crate::{";

    while let Some((i, c)) = chars.next() {
        // Check if we're at the start of a grouped import
        if source[i..].starts_with(pattern) {
            // Skip past "use crate::{"
            for _ in 0..pattern.len() - 1 {
                chars.next();
            }

            // Find the matching closing brace
            let start = i + pattern.len();
            let mut depth = 1;
            let mut end = start;

            for (j, ch) in source[start..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = start + j;
                            break;
                        }
                    }
                    _ => {}
                }
            }

            let items_str = &source[start..end];

            // Skip past the content and closing brace + semicolon
            // We need to advance chars iterator past this
            while let Some(&(pos, _)) = chars.peek() {
                if pos > end {
                    break;
                }
                chars.next();
            }
            // Skip the semicolon if present
            if let Some(&(_, ';')) = chars.peek() {
                chars.next();
            }

            // Split and categorize items
            let items = split_use_items(items_str);
            let mut module_items: Vec<String> = Vec::new();
            let mut root_items: Vec<String> = Vec::new();

            for item in items {
                let item = item.trim();
                if item.is_empty() {
                    continue;
                }

                // Get the first identifier
                let ident = item
                    .split(|c: char| c == ':' || c.is_whitespace())
                    .next()
                    .unwrap_or(item)
                    .trim();

                if exported_macros.contains(ident) || ident == module_name {
                    root_items.push(item.to_string());
                } else {
                    module_items.push(item.to_string());
                }
            }

            // Generate the new imports
            if !root_items.is_empty() {
                if root_items.len() == 1 {
                    result.push_str(&format!("use crate::{};", root_items[0]));
                } else {
                    result.push_str(&format!("use crate::{{{}}};", root_items.join(", ")));
                }
            }
            if !root_items.is_empty() && !module_items.is_empty() {
                result.push('\n');
            }
            if !module_items.is_empty() {
                if module_items.len() == 1 {
                    result.push_str(&format!("use crate::{}::{};", module_name, module_items[0]));
                } else {
                    result.push_str(&format!(
                        "use crate::{}::{{{}}};",
                        module_name,
                        module_items.join(", ")
                    ));
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Split use items by comma, respecting nested braces
/// e.g., "color::{A, B}, flags::Flags" -> ["color::{A, B}", "flags::Flags"]
fn split_use_items(s: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut depth = 0;
    let mut start = 0;

    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                let item = s[start..i].trim();
                if !item.is_empty() {
                    items.push(item.to_string());
                }
                start = i + 1;
            }
            _ => {}
        }
    }

    // Don't forget the last item
    let item = s[start..].trim();
    if !item.is_empty() {
        items.push(item.to_string());
    }

    items
}

/// Find #[macro_export] macro names in source
fn find_exported_macros(source: &str) -> std::collections::HashSet<String> {
    let mut macros = std::collections::HashSet::new();
    let pattern =
        regex::Regex::new(r"#\[macro_export\]\s*macro_rules!\s*([a-zA-Z_][a-zA-Z0-9_]*)").unwrap();

    for caps in pattern.captures_iter(source) {
        if let Some(name) = caps.get(1) {
            macros.insert(name.as_str().to_string());
        }
    }

    macros
}

/// Find dependency renames from Cargo.toml (package = "X" renames)
fn find_dependency_renames(crate_src_dir: &Path) -> std::collections::HashMap<String, String> {
    let mut renames = std::collections::HashMap::new();

    // Go up from src/ to crate root
    let crate_root = crate_src_dir.parent().unwrap_or(crate_src_dir);
    let cargo_toml = crate_root.join("Cargo.toml");

    if let Ok(content) = fs::read_to_string(&cargo_toml) {
        // Parse for renamed dependencies: package = "actual_name"
        // Maps: import_name -> actual_package_name
        // e.g., [dependencies.users] package = "uzers" -> users -> uzers
        // Also handles [target."cfg(unix)".dependencies.users] package = "uzers"

        // Look for sections like [dependencies.X] or [target."cfg(...)".dependencies.X]
        let section_pattern = regex::Regex::new(
            r#"\[(?:target\.[^\]]+\.)?dependencies\.([a-zA-Z_][a-zA-Z0-9_-]*)\]"#,
        )
        .unwrap();
        let package_pattern = regex::Regex::new(r#"package\s*=\s*"([^"]+)""#).unwrap();
        // Check if we hit a new section (not a key-value pair)
        let new_section_pattern = regex::Regex::new(r"^\s*\[").unwrap();

        let mut current_dep_name: Option<String> = None;

        for line in content.lines() {
            if let Some(caps) = section_pattern.captures(line) {
                current_dep_name = Some(caps.get(1).unwrap().as_str().to_string());
            } else if new_section_pattern.is_match(line) {
                // New section that isn't a dependency section - reset
                current_dep_name = None;
            } else if let Some(ref dep_name) = current_dep_name {
                if let Some(caps) = package_pattern.captures(line) {
                    let actual_package = caps.get(1).unwrap().as_str().to_string();
                    if dep_name != &actual_package {
                        renames.insert(dep_name.clone(), actual_package);
                    }
                }
            }
        }
    }

    renames
}

/// Transform `crate::X` to `crate::{module}::X` in source files
/// Also handles:
/// - extern crate declarations with renamed packages
/// - Avoiding transforms for #[macro_export] macros (they live at crate root)
fn transform_crate_references(
    source: &str,
    module_name: &str,
    exported_macros: &std::collections::HashSet<String>,
    dep_renames: &std::collections::HashMap<String, String>,
) -> String {
    let mut result = source.to_string();

    // 1. Handle extern crate declarations with renamed packages
    // Transform: extern crate users; -> extern crate uzers as users;
    for (import_name, actual_package) in dep_renames {
        let extern_pattern = regex::Regex::new(&format!(
            r"extern\s+crate\s+{}\s*;",
            regex::escape(import_name)
        ))
        .unwrap();

        if extern_pattern.is_match(&result) {
            result = extern_pattern
                .replace_all(
                    &result,
                    &format!("extern crate {} as {};", actual_package, import_name),
                )
                .to_string();
        }

        // Also transform use statements for renamed packages
        // Transform: use users::X -> use uzers::X
        let use_pattern =
            regex::Regex::new(&format!(r"\buse\s+{}::", regex::escape(import_name))).unwrap();

        if use_pattern.is_match(&result) {
            result = use_pattern
                .replace_all(&result, &format!("use {}::", actual_package))
                .to_string();
        }
    }
    // 2. Transform `use crate::{A, B, C}` grouped imports (including multi-line)
    // We need to handle nested braces like `use crate::{color::{A, B}, flags::Flags}`
    // Do a manual search for `use crate::{` and find the matching closing brace
    result = transform_grouped_crate_imports(&result, module_name, exported_macros);

    // 3. Transform `use crate::X` to `use crate::{module}::X` (simple non-grouped imports)
    // But NOT for exported macros (they stay at crate root)
    let use_pattern = regex::Regex::new(r"\buse\s+crate::([a-zA-Z_][a-zA-Z0-9_]*)").unwrap();

    result = use_pattern
        .replace_all(&result, |caps: &regex::Captures| {
            let full_match = caps.get(0).unwrap().as_str();
            let ident = caps.get(1).unwrap().as_str();

            // Don't transform if it's already our module name
            if ident == module_name {
                return full_match.to_string();
            }

            // Don't transform if it's an exported macro (macros stay at crate root)
            if exported_macros.contains(ident) {
                return full_match.to_string();
            }

            // Transform: use crate::X -> use crate::{module}::X
            format!("use crate::{}::{}", module_name, ident)
        })
        .to_string();

    // 4. Transform `crate::X::` paths (not in use statements) to `crate::{module}::X::`
    // This catches type paths like `crate::meta::Permissions`
    let path_pattern = regex::Regex::new(r"\bcrate::([a-zA-Z_][a-zA-Z0-9_]*)::").unwrap();

    result = path_pattern
        .replace_all(&result, |caps: &regex::Captures| {
            let full_match = caps.get(0).unwrap().as_str();
            let ident = caps.get(1).unwrap().as_str();

            // Don't transform if it's already our module name
            if ident == module_name {
                return full_match.to_string();
            }

            // Don't transform if it's an exported macro
            if exported_macros.contains(ident) {
                return full_match.to_string();
            }

            // Transform: crate::X:: -> crate::{module}::X::
            format!("crate::{}::{}::", module_name, ident)
        })
        .to_string();

    result
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

/// Generate a wrapper for uucore::bin! crates (uutils coreutils utilities)
fn generate_uucore_wrapper(uucore_crate_name: &str, cmd_name: &str) -> String {
    let sanitized = sanitize_name(cmd_name);
    // The uucore crate exports `uumain` which takes Args and returns i32
    // We generate a wrapper that calls it with our args
    format!(
        r#"/// Auto-generated wrapper for {crate_name}
pub fn rustbb_cmd_{sanitized}() -> i32 {{
    {crate_name}::uumain(rustbb_runtime::args_os())
}}
"#,
        crate_name = uucore_crate_name,
        sanitized = sanitized,
    )
}
