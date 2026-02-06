//! Runtime library for rustbb multi-call binaries.
//!
//! This crate provides the command registry and dispatch mechanism
//! for combined multi-call binaries created by rustbb.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::Mutex;

// Global storage for the effective arguments.
// Using a Mutex so all threads (rayon, tokio, etc.) see the same args.
// The previous thread_local RefCell approach silently fell back to
// raw std::env::args() on spawned threads, producing wrong argv.
static EFFECTIVE_ARGS: Mutex<Option<Vec<OsString>>> = Mutex::new(None);

/// Returns an iterator over the arguments, with proper shifting for multi-call mode.
///
/// In subcommand mode (`./mybox cmd args...`), this returns `["cmd", "args", ...]`
/// In symlink mode (`./cmd args...`), this returns `["cmd", "args", ...]`
///
/// This is a drop-in replacement for `std::env::args()` that handles multi-call binaries correctly.
/// Safe to call from any thread.
pub fn args() -> impl Iterator<Item = String> {
    args_os().map(|s| s.to_string_lossy().into_owned())
}

/// Returns an iterator over the arguments as OsStrings.
///
/// This is a drop-in replacement for `std::env::args_os()` that handles multi-call binaries correctly.
/// Safe to call from any thread.
pub fn args_os() -> impl Iterator<Item = OsString> {
    let guard = EFFECTIVE_ARGS.lock().unwrap_or_else(|e| e.into_inner());
    let args = guard
        .clone()
        .unwrap_or_else(|| std::env::args_os().collect());
    args.into_iter()
}

/// Set the effective arguments for the current command.
/// This is called by the dispatcher before invoking a command.
fn set_effective_args(args: Vec<OsString>) {
    let mut guard = EFFECTIVE_ARGS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(args);
}

/// Clear the effective arguments after a command completes.
fn clear_effective_args() {
    let mut guard = EFFECTIVE_ARGS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// Drop guard that clears effective args when dropped.
/// Ensures cleanup even if the command panics (and the panic is caught).
struct ArgsGuard;

impl Drop for ArgsGuard {
    fn drop(&mut self) {
        clear_effective_args();
    }
}

/// Function signature for command entry points.
///
/// Commands should return an exit code (0 for success, non-zero for errors).
pub type CommandFn = fn() -> i32;

/// Registry of available commands.
///
/// Commands register themselves at startup and the dispatcher
/// looks up the appropriate command based on how the binary was invoked.
pub struct Registry {
    commands: HashMap<String, CommandFn>,
}

impl Registry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    /// Register a command with the given name.
    pub fn register(&mut self, name: &str, func: CommandFn) {
        self.commands.insert(name.to_string(), func);
    }

    /// Get a command function by name.
    pub fn get(&self, name: &str) -> Option<&CommandFn> {
        self.commands.get(name)
    }

    /// List all registered command names.
    pub fn list(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.commands.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Try to run a command by name.
    ///
    /// Returns `Some(exit_code)` if the command was found and executed,
    /// `None` if no command with that name exists.
    pub fn try_run(&self, name: &str) -> Option<i32> {
        self.commands.get(name).map(|cmd| cmd())
    }

    /// Try to run a command with specific arguments.
    ///
    /// This sets up the effective arguments before calling the command,
    /// so that `rustbb_runtime::args()` returns the correct values.
    /// The args are visible to all threads spawned by the command.
    pub fn try_run_with_args(&self, name: &str, args: Vec<OsString>) -> Option<i32> {
        self.commands.get(name).map(|cmd| {
            set_effective_args(args);
            let _guard = ArgsGuard; // clears args on drop, even if cmd() panics
            cmd()
        })
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Dispatch to the appropriate command based on argv.
///
/// This function determines which command to run based on:
/// 1. The binary name (argv\[0\]) - for symlink-based invocation
/// 2. The first argument (argv\[1\]) - for subcommand-based invocation
///
/// # Symlink mode
/// ```bash
/// ln -s rustbb cat
/// ./cat file.txt  # Runs the "cat" command
/// ```
///
/// # Subcommand mode
/// ```bash
/// ./rustbb cat file.txt  # Runs the "cat" command
/// ```
///
/// This function never returns - it exits the process with the command's exit code.
pub fn dispatch(registry: &Registry) -> ! {
    let args: Vec<OsString> = std::env::args_os().collect();
    let args_str: Vec<String> = args
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect();

    // Get the binary name (argv[0])
    let binary_path = Path::new(&args_str[0]);
    let binary_name = binary_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("rustbb");

    // Strategy 1: Try binary name (symlink mode)
    // e.g., `ln -s rustbb cat && ./cat file.txt`
    // In this mode, args are already correct: ["cat", "file.txt"]
    if registry.get(binary_name).is_some() {
        // Symlink mode - args are already correct
        let effective_args = args.clone();
        if let Some(exit_code) = registry.try_run_with_args(binary_name, effective_args) {
            std::process::exit(exit_code);
        }
    }

    // Strategy 2: Try first argument (subcommand mode)
    // e.g., `./rustbb cat file.txt`
    if args_str.len() > 1 {
        let cmd_name = &args_str[1];

        // Check for help flags
        if cmd_name == "--help" || cmd_name == "-h" {
            print_help(binary_name, registry);
            std::process::exit(0);
        }

        // Check for list command
        if cmd_name == "--list" || cmd_name == "-l" {
            for cmd in registry.list() {
                println!("{}", cmd);
            }
            std::process::exit(0);
        }

        if registry.get(cmd_name).is_some() {
            // Subcommand mode - shift args so command sees ["cmd", "arg1", "arg2", ...]
            // instead of ["mybox", "cmd", "arg1", "arg2", ...]
            let effective_args: Vec<OsString> = args[1..].to_vec();
            if let Some(exit_code) = registry.try_run_with_args(cmd_name, effective_args) {
                std::process::exit(exit_code);
            }
        }

        // Command not found
        eprintln!("Unknown command: {}", cmd_name);
        eprintln!();
        print_help(binary_name, registry);
        std::process::exit(1);
    }

    // No command specified - show help
    print_help(binary_name, registry);
    std::process::exit(1);
}

fn print_help(binary_name: &str, registry: &Registry) {
    eprintln!("Usage: {} <command> [args...]", binary_name);
    eprintln!("       <command> [args...]  (via symlink)");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -h, --help    Show this help message");
    eprintln!("  -l, --list    List available commands");
    eprintln!();
    eprintln!("Available commands:");
    for cmd in registry.list() {
        eprintln!("  {}", cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_basic() {
        fn test_cmd() -> i32 {
            42
        }

        let mut registry = Registry::new();
        registry.register("test", test_cmd);

        assert!(registry.get("test").is_some());
        assert!(registry.get("nonexistent").is_none());
        assert_eq!(registry.try_run("test"), Some(42));
    }

    #[test]
    fn test_registry_list() {
        fn cmd_a() -> i32 {
            0
        }
        fn cmd_b() -> i32 {
            0
        }

        let mut registry = Registry::new();
        registry.register("beta", cmd_b);
        registry.register("alpha", cmd_a);

        let list = registry.list();
        assert_eq!(list, vec!["alpha", "beta"]);
    }

    // Tests that touch global EFFECTIVE_ARGS must not run in parallel.
    // We use a shared test mutex to serialize them.
    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_args_default_fallback() {
        let _lock = TEST_MUTEX.lock().unwrap();
        // When no effective args are set, args_os() should return std::env::args_os()
        clear_effective_args();
        let result: Vec<OsString> = args_os().collect();
        let expected: Vec<OsString> = std::env::args_os().collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_args_with_effective() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let test_args = vec![
            OsString::from("test-cmd"),
            OsString::from("--flag"),
            OsString::from("value"),
        ];
        set_effective_args(test_args.clone());
        let result: Vec<OsString> = args_os().collect();
        assert_eq!(result, test_args);
        clear_effective_args();
    }

    #[test]
    fn test_args_visible_from_thread() {
        let _lock = TEST_MUTEX.lock().unwrap();
        let test_args = vec![OsString::from("threaded-cmd"), OsString::from("arg1")];
        set_effective_args(test_args.clone());

        let handle = std::thread::spawn(|| {
            let result: Vec<OsString> = args_os().collect();
            result
        });

        let thread_result = handle.join().unwrap();
        assert_eq!(thread_result, test_args);
        clear_effective_args();
    }
}
