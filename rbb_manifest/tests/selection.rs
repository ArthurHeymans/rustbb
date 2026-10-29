use rbb_manifest::Manifest;

#[test]
fn rich_sh_selection_keeps_its_external_posix_companions() {
    for coreutils in [false, true] {
        let manifest = Manifest::with_tools("aarch64-unknown-linux-musl", coreutils, true);
        manifest.validate().unwrap();
        for preset in ["minimal", "rescue", "diskboot", "netboot"] {
            let selected = manifest.select(preset, &[]).unwrap();
            for required in ["printf", "test", "["] {
                assert!(selected.iter().any(|n| n == required), "{preset}: {required}");
            }
        }
        let selected = manifest.select("minimal", &["init".into(), "sh".into()]).unwrap();
        assert_eq!(selected.len(), 5);
    }
    let minimal = Manifest::current("aarch64-unknown-linux-musl");
    assert!(!minimal.commands.iter().any(|n| n == "printf"));
}

#[test]
fn bracket_is_the_only_special_character_command_exception() {
    for name in ["[", "]", "../test", "[foo]", "foo/bar", "foo\nbar"] {
        let mut manifest = Manifest::current("x86_64-unknown-linux-musl");
        manifest.commands.push(name.into());
        assert_eq!(manifest.validate().is_ok(), name == "[");
    }
}
