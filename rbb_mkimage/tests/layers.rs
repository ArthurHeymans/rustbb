mod common;
use rbb_mkimage::{Conflict, Entry, Image};
use std::fs;
use std::process::Command;

#[test]
fn manifest_target_and_missing_inventory_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    for missing in [false, true] {
        let mut elf = common::elf();
        if missing {
            let offset = elf.windows(13).position(|s| s == b".rbb.commands").unwrap();
            elf[offset] = b'x';
        } else {
            elf[18] = 183;
        }
        let binary = dir.path().join("bad");
        fs::write(&binary, elf).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_rbb-mkimage"))
            .arg("--busybox")
            .arg(binary)
            .arg("-o")
            .arg(dir.path().join("output"))
            .output()
            .unwrap();
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains(if missing {
                "no command manifest"
            } else {
                "differs from ELF"
            }),
            "{error}"
        );
    }
}

#[test]
fn explicit_commands_and_failed_merge_preserve_output() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("rbb");
    fs::write(&binary, common::elf()).unwrap();
    let config = dir.path().join("config");
    fs::write(
        &config,
        "[[services]]\nname='ctl'\nkind='oneshot'\ncommand=['/bin/ctl','status']\n",
    )
    .unwrap();
    let output = dir.path().join("image");
    let build = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_rbb-mkimage"));
        c.arg("--busybox")
            .arg(&binary)
            .arg("--commands")
            .arg("init,ctl")
            .arg("--compress")
            .arg("none")
            .arg("--file")
            .arg(format!("{}:/etc/rbb/init.toml", config.display()))
            .arg("-o")
            .arg(&output);
        c
    };
    assert!(build().status().unwrap().success());
    let before = fs::read(&output).unwrap();
    let image = Image::read_archive(&before, Conflict::Error).unwrap();
    assert!(image.entries.contains_key("bin/ctl"));
    assert!(!image.entries.contains_key("bin/sh"));
    let mut base = Image::default();
    base.insert("init", Entry::file(vec![0], 0o755)).unwrap();
    let path = dir.path().join("base");
    base.write_newc(fs::File::create(&path).unwrap()).unwrap();
    assert!(!build()
        .arg("--base")
        .arg(&path)
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(fs::read(&output).unwrap(), before);
    // A failed import must not truncate/replace a previously-built output.
    fs::write(&path, b"070701ffffffff").unwrap();
    assert!(!build()
        .arg("--base")
        .arg(path)
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(fs::read(&output).unwrap(), before);
}
