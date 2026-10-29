#![cfg(feature = "brush")]
use std::os::unix::fs::symlink;
use std::process::Command;

#[test]
fn sh_mode_has_posix_printf_without_host_tools() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["printf", "test", "["] {
        symlink(env!("CARGO_BIN_EXE_rbb"), dir.path().join(name)).unwrap();
    }
    let result = Command::new(env!("CARGO_BIN_EXE_rbb"))
        .args([
            "sh",
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "[ \"$1\" = one ] && test -n \"$0\" && printf '%s:%s' \"$0\" \"$1\"",
            "zero",
            "one",
        ])
        .env("PATH", dir.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"zero:one");
}
