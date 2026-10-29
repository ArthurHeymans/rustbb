fn main() {
    let manifest = rbb_manifest::Manifest::current(&std::env::var("TARGET").unwrap());
    manifest.validate().unwrap();
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let generated = format!(
        "#[used]\n#[link_section = \"{}\"]\nstatic COMMAND_MANIFEST: [u8; {}] = {:?};\n",
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
