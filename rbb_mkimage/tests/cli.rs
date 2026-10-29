use std::fs;
use std::process::Command;

mod common;
use common::elf;

#[test]
fn reproducible_cli_and_failed_build_preserves_output() {
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("rbb");
    let first = directory.path().join("one.gz");
    let second = directory.path().join("two.gz");
    fs::write(&binary, elf()).unwrap();
    for output in [&first, &second] {
        assert!(Command::new(env!("CARGO_BIN_EXE_rbb-mkimage"))
            .arg("--busybox")
            .arg(&binary)
            .arg("-o")
            .arg(output)
            .status()
            .unwrap()
            .success());
    }
    let original = fs::read(&first).unwrap();
    assert_eq!(original, fs::read(second).unwrap());
    let config = directory.path().join("invalid.toml");
    fs::write(
        &config,
        "[[services]]\nname='x'\ncommand=['/x']\nafter=['x']",
    )
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_rbb-mkimage"))
        .arg("--busybox")
        .arg(&binary)
        .arg("-o")
        .arg(&first)
        .arg("--file")
        .arg(format!("{}:/etc/rbb/init.toml", config.display()))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(fs::read(&first).unwrap(), original);
    assert!(String::from_utf8_lossy(&result.stderr).contains("cycle"));
    let result = Command::new(env!("CARGO_BIN_EXE_rbb-mkimage"))
        .arg("--busybox")
        .arg(&binary)
        .arg("-o")
        .arg(&first)
        .arg("--file")
        .arg(format!("{}:/bbin/rbb", binary.display()))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(fs::read(first).unwrap(), original);
}

#[test]
fn dynamic_binary_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("dynamic");
    let mut data = elf();
    data[64] = 3; // PT_INTERP
    fs::write(&binary, data).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_rbb-mkimage"))
        .arg("--busybox")
        .arg(binary)
        .arg("-o")
        .arg(directory.path().join("bad.gz"))
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("PT_INTERP"));
}
