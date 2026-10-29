fn main() {
    let manifest = rbb_manifest::Manifest::with_tools(
        &std::env::var("TARGET").unwrap(),
        std::env::var_os("CARGO_FEATURE_COREUTILS").is_some(),
        std::env::var_os("CARGO_FEATURE_BRUSH").is_some(),
    );
    manifest.validate().unwrap();
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let generated = format!(
        "pub const APPLETS: &[&str] = &{:?};\n#[used]\n#[link_section = \"{}\"]\nstatic COMMAND_MANIFEST: [u8; {}] = {:?};\n",
        manifest.commands,
        rbb_manifest::SECTION,
        bytes.len(),
        bytes
    );
    std::fs::write(
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("commands.rs"),
        generated,
    )
    .unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
