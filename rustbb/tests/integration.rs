//! Integration tests for rustbb.
//!
//! These tests build actual multi-call binaries from the example crates
//! and verify they work correctly in both subcommand and symlink mode.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Get the workspace root directory
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Build a multi-call binary from the given example crate paths.
/// Returns the path to the built binary.
fn build_busybox(example_names: &[&str], output_name: &str) -> PathBuf {
    let root = workspace_root();
    let output_dir = root.join("target").join("test-outputs");
    fs::create_dir_all(&output_dir).unwrap();

    let output_path = output_dir.join(output_name);

    // Build the example paths
    let example_paths: Vec<String> = example_names
        .iter()
        .map(|name| {
            root.join("examples")
                .join(name)
                .to_string_lossy()
                .to_string()
        })
        .collect();

    // rustbb uses `-o` as both the package name and the output file path
    // (relative to cwd), so we set cwd to the output dir and pass just the name.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rustbb"));
    cmd.arg("build");
    for path in &example_paths {
        cmd.arg(path);
    }
    cmd.arg("-o").arg(output_name).current_dir(&output_dir);

    let output = cmd.output().expect("Failed to run rustbb build");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "rustbb build failed:\nstdout: {}\nstderr: {}",
            stdout, stderr
        );
    }

    assert!(
        output_path.exists(),
        "Binary was not created at {}",
        output_path.display()
    );
    output_path
}

/// Run a binary in subcommand mode: `./binary <command> [args...]`
fn run_subcommand(binary: &Path, cmd: &str, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(binary);
    command.arg(cmd);
    for arg in args {
        command.arg(arg);
    }
    command.output().expect("Failed to execute binary")
}

/// Run a binary in symlink mode: create a symlink named `cmd` -> binary, then run it
fn run_via_symlink(binary: &Path, cmd: &str, args: &[&str]) -> std::process::Output {
    let symlink_dir = binary.parent().unwrap();
    let symlink_path = symlink_dir.join(cmd);

    // Remove existing symlink if present
    let _ = fs::remove_file(&symlink_path);

    // Create symlink
    #[cfg(unix)]
    std::os::unix::fs::symlink(binary, &symlink_path).expect("Failed to create symlink");

    let mut command = Command::new(&symlink_path);
    for arg in args {
        command.arg(arg);
    }
    let output = command.output().expect("Failed to execute symlink");

    // Cleanup
    let _ = fs::remove_file(&symlink_path);

    output
}

// ---- Tests ----

#[test]
fn test_build_simple_examples() {
    // Build a busybox with simple_echo and simple_cat
    let binary = build_busybox(&["simple_echo", "simple_cat"], "test_basic_bb");
    assert!(binary.exists());
}

#[test]
fn test_echo_subcommand_mode() {
    let binary = build_busybox(&["simple_echo"], "test_echo_sub");

    let output = run_subcommand(&binary, "simple_echo", &["hello", "world"]);
    assert!(output.status.success(), "Command failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "hello world");
}

#[test]
fn test_echo_symlink_mode() {
    let binary = build_busybox(&["simple_echo"], "test_echo_sym");

    let output = run_via_symlink(&binary, "simple_echo", &["hello", "symlink"]);
    assert!(output.status.success(), "Symlink command failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "hello symlink");
}

#[test]
fn test_cat_reads_file() {
    let binary = build_busybox(&["simple_cat"], "test_cat_file");

    // Create a temp file to cat
    let test_file = binary.parent().unwrap().join("test_cat_input.txt");
    fs::write(&test_file, "line1\nline2\nline3\n").unwrap();

    let output = run_subcommand(&binary, "simple_cat", &[test_file.to_str().unwrap()]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout, "line1\nline2\nline3\n");

    // Cleanup
    let _ = fs::remove_file(&test_file);
}

#[test]
fn test_head_with_lines_flag() {
    let binary = build_busybox(&["head"], "test_head_lines");

    // Create a temp file
    let test_file = binary.parent().unwrap().join("test_head_input.txt");
    let content: String = (1..=20).map(|i| format!("line {}\n", i)).collect();
    fs::write(&test_file, &content).unwrap();

    let output = run_subcommand(&binary, "head", &["-n", "3", test_file.to_str().unwrap()]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout, "line 1\nline 2\nline 3\n");

    // Cleanup
    let _ = fs::remove_file(&test_file);
}

#[test]
fn test_wc_counts() {
    let binary = build_busybox(&["wc"], "test_wc_counts");

    // Create a temp file
    let test_file = binary.parent().unwrap().join("test_wc_input.txt");
    fs::write(&test_file, "hello world\nfoo bar baz\n").unwrap();

    // Count lines only
    let output = run_subcommand(&binary, "wc", &["-l", test_file.to_str().unwrap()]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("2"), "Expected 2 lines, got: {}", stdout);

    // Cleanup
    let _ = fs::remove_file(&test_file);
}

#[test]
fn test_async_hello() {
    let binary = build_busybox(&["async_hello"], "test_async_hello");

    let output = run_subcommand(&binary, "async_hello", &["testing", "async"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Starting..."), "Missing 'Starting...'");
    assert!(stdout.contains("testing async"), "Missing message");
    assert!(stdout.contains("Done!"), "Missing 'Done!'");
}

#[test]
fn test_multi_command_binary() {
    // Build a busybox with multiple commands
    let binary = build_busybox(
        &["simple_echo", "simple_cat", "head", "wc"],
        "test_multi_cmd",
    );

    // Test each command works in the same binary
    let echo_out = run_subcommand(&binary, "simple_echo", &["multi", "test"]);
    assert!(echo_out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&echo_out.stdout).trim(),
        "multi test"
    );

    // Test --list flag
    let list_out = run_subcommand(&binary, "--list", &[]);
    assert!(list_out.status.success());
    let list_stdout = String::from_utf8_lossy(&list_out.stdout);
    assert!(list_stdout.contains("simple_echo"));
    assert!(list_stdout.contains("simple_cat"));
    assert!(list_stdout.contains("head"));
    assert!(list_stdout.contains("wc"));
}

#[test]
fn test_unknown_command_fails() {
    let binary = build_busybox(&["simple_echo"], "test_unknown_cmd");

    let output = run_subcommand(&binary, "nonexistent_command", &[]);
    assert!(!output.status.success(), "Unknown command should fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Unknown command"));
}

#[test]
fn test_help_flag() {
    let binary = build_busybox(&["simple_echo"], "test_help_flag");

    let output = run_subcommand(&binary, "--help", &[]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Usage:"));
    assert!(stderr.contains("simple_echo"));
}
