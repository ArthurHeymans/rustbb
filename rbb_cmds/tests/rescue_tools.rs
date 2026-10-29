#![cfg(feature = "rescue-tools")]
use std::fs;
use std::os::unix::fs::{symlink, FileTypeExt};
use std::process::{Command, Output};

fn rbb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rbb"))
        .args(args)
        .output()
        .unwrap()
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn every_selected_uutils_library_has_working_help_and_registration() {
    for name in rbb_manifest::COREUTILS_EXTRA
        .iter()
        .chain(["cat", "ls", "cp", "rm", "mkdir", "ln", "chmod", "readlink"].iter())
    {
        if ["test", "["].contains(name) {
            continue;
        }
        let output = rbb(&[name, "--help"]);
        success(&output);
        assert!(!output.stdout.is_empty(), "{name}");
    }
}

#[test]
fn richer_file_text_hash_and_timeout_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    fs::write(&source, "beta\nalpha\nalpha\n").unwrap();
    let path = source.to_str().unwrap();
    assert_eq!(rbb(&["head", "-n", "1", path]).stdout, b"beta\n");
    assert_eq!(rbb(&["tail", "-n", "1", path]).stdout, b"alpha\n");
    assert_eq!(rbb(&["sort", "-u", path]).stdout, b"alpha\nbeta\n");
    assert_eq!(
        String::from_utf8(rbb(&["wc", "-l", path]).stdout)
            .unwrap()
            .split_whitespace()
            .next(),
        Some("3")
    );
    let empty = dir.path().join("empty");
    fs::write(&empty, "").unwrap();
    let hash = rbb(&["sha256sum", empty.to_str().unwrap()]);
    success(&hash);
    assert!(hash
        .stdout
        .starts_with(b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"));
    let tree = dir.path().join("tree");
    fs::create_dir(&tree).unwrap();
    fs::write(tree.join("child"), "ok").unwrap();
    let copy = dir.path().join("copy");
    success(&rbb(&[
        "cp",
        "-r",
        tree.to_str().unwrap(),
        copy.to_str().unwrap(),
    ]));
    assert_eq!(fs::read(copy.join("child")).unwrap(), b"ok");
    success(&rbb(&["rm", "-r", copy.to_str().unwrap()]));
    assert!(!copy.exists());
    let fifo = dir.path().join("fifo");
    success(&rbb(&["mkfifo", fifo.to_str().unwrap()]));
    assert!(fs::metadata(fifo).unwrap().file_type().is_fifo());
    assert_eq!(
        rbb(&["timeout", "0.05", env!("CARGO_BIN_EXE_rbb"), "sleep", "2"])
            .status
            .code(),
        Some(124)
    );
}

#[test]
fn brush_subcommands_and_symlinks_preserve_shell_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_rbb");
    let code = "set -eu; f() { printf '%s\\n' \"$1\"; }; x=$(f beta); for v in \"$x\" alpha alpha; do printf '%s\\n' \"$v\"; done | \"$RBB\" sort -u > result; test $(\"$RBB\" wc -l < result) -eq 2; export PROOF=works; test $(\"$RBB\" printenv PROOF) = works; printf 'BRUSH_PASS\\n'";
    let result = Command::new(binary)
        .args(["brush", "--no-config", "--noprofile", "--norc", "-c", code])
        .current_dir(dir.path())
        .env("RBB", binary)
        .output()
        .unwrap();
    success(&result);
    assert_eq!(result.stdout, b"BRUSH_PASS\n");
    for name in ["brush", "sh"] {
        let link = dir.path().join(name);
        symlink(binary, &link).unwrap();
        let output = Command::new(link)
            .args([
                "--no-config",
                "--noprofile",
                "--norc",
                "-c",
                "printf '%s:%s' \"$0\" \"$1\"",
                "chosen-zero",
                "argument one",
            ])
            .output()
            .unwrap();
        success(&output);
        assert_eq!(output.stdout, b"chosen-zero:argument one");
    }
    assert_eq!(rbb(&["sh", "-c", "exit 17"]).status.code(), Some(17));
    assert_eq!(rbb(&["sh", "-c", "printf '%s' \"$0\""]).stdout, b"sh");
    assert_eq!(
        rbb(&["minish", "-c", "echo fallback"]).stdout,
        b"fallback\n"
    );
}
