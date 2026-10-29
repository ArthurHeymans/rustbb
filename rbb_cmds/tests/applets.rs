use std::fs;
use std::os::unix::fs::symlink;
use std::process::{Command, Output};

fn rbb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rbb"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn dispatch_and_exit_status() {
    let listing = rbb(&["--list"]);
    assert!(listing.status.success());
    let text = String::from_utf8(listing.stdout).unwrap();
    assert_eq!(text.lines().collect::<Vec<_>>(), rbb_cmds::APPLETS);
    assert_eq!(rbb(&["echo", "hello", "world"]).stdout, b"hello world\n");
    assert!(rbb(&["true"]).status.success());
    assert_eq!(rbb(&["false"]).status.code(), Some(1));
    assert!(!rbb(&["not-an-applet"]).status.success());
    assert!(!rbb(&["sleep", "NaN"]).status.success());
    assert!(!rbb(&["mount", "-t"]).status.success());
    assert!(!rbb(&["init", "--unknown"]).status.success());
    let directory = tempfile::tempdir().unwrap();
    let echo = directory.path().join("echo");
    symlink(env!("CARGO_BIN_EXE_rbb"), &echo).unwrap();
    assert_eq!(
        Command::new(echo)
            .args(["via", "symlink"])
            .output()
            .unwrap()
            .stdout,
        b"via symlink\n"
    );
}

#[test]
fn interpreter_and_files() {
    assert_eq!(
        rbb(&["sh", "-c", "echo 'a b' \"\" c\\ d # comment"]).stdout,
        b"a b  c d\n"
    );
    assert_eq!(rbb(&["sh", "-c", "exit 7"]).status.code(), Some(7));
    assert!(!rbb(&["sh", "-c", "echo '"]).status.success());
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let copy = directory.path().join("copy");
    fs::write(&source, "hello\n").unwrap();
    assert!(
        rbb(&["cp", source.to_str().unwrap(), copy.to_str().unwrap()])
            .status
            .success()
    );
    assert_eq!(rbb(&["cat", copy.to_str().unwrap()]).stdout, b"hello\n");
    assert!(rbb(&["rm", copy.to_str().unwrap()]).status.success());
    assert!(!copy.exists());
}
