//! Adversarial regressions: successful builds must preserve command semantics.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

struct Fixture(TempDir);
impl Fixture {
    fn new() -> Self {
        Self(tempfile::tempdir().unwrap())
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn file(&self, path: &str, text: &str) {
        let path = self.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    fn package(&self, name: &str, source: &str, manifest: &str) -> PathBuf {
        self.file(
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname={name:?}\nversion='0.1.0'\nedition='2021'\n{manifest}"),
        );
        self.file(&format!("{name}/src/main.rs"), source);
        self.path().join(name)
    }
    fn build(&self, packages: &[&str], flags: &[&str]) -> PathBuf {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rustbb"));
        cmd.arg("build");
        for package in packages {
            cmd.arg(self.path().join(package));
        }
        cmd.args(["-o", "combined"])
            .args(flags)
            .current_dir(self.path())
            .env("CARGO_NET_OFFLINE", "true");
        checked(&mut cmd);
        self.path().join("combined")
    }
}
fn checked(cmd: &mut Command) -> Output {
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{cmd:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn run(binary: &Path, command: &str, args: &[&str]) -> Output {
    Command::new(binary)
        .arg(command)
        .args(args)
        .output()
        .unwrap()
}
fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap().trim()
}

#[test]
fn preserves_binary_package_metadata_and_builds_release_output() {
    let f = Fixture::new();
    f.package(
        "metadata",
        "fn main(){println!(\"{} {}\",env!(\"CARGO_PKG_NAME\"),env!(\"CARGO_PKG_VERSION\"));}",
        "default-run='metadata'",
    );
    let binary = f.build(&["metadata"], &["--release"]);
    assert_eq!(stdout(&run(&binary, "metadata", &[])), "metadata 0.1.0");
}

#[test]
fn preserves_default_implied_and_path_dependency_features_without_cross_command_leakage() {
    let f = Fixture::new();
    f.file("featurelib/Cargo.toml", "[package]\nname='featurelib'\nversion='0.1.0'\nedition='2021'\n[features]\ndefault=['extra']\nextra=[]\nrequired=[]\n");
    f.file(
        "featurelib/src/lib.rs",
        "pub fn flags()->(bool,bool){(cfg!(feature=\"extra\"),cfg!(feature=\"required\"))}",
    );
    f.package("enabled", "fn main(){println!(\"{} {} {:?}\",cfg!(feature=\"enabled\"),cfg!(feature=\"implied\"),alias::flags());}", "[features]\ndefault=['enabled']\nenabled=['implied']\nimplied=[]\nselected=['implied']\n[dependencies]\nalias={package='featurelib',path='../featurelib',default-features=false,features=['required']}\n");
    f.package(
        "disabled",
        "fn main(){println!(\"{}\",cfg!(feature=\"enabled\"));}",
        "[features]\nenabled=[]\n",
    );
    let binary = f.build(&["enabled", "disabled"], &[]);
    assert_eq!(
        stdout(&run(&binary, "enabled", &[])),
        "true true (false, true)"
    );
    assert_eq!(stdout(&run(&binary, "disabled", &[])), "false");
    let binary = f.build(&["enabled"], &["--no-default-features"]);
    assert_eq!(
        stdout(&run(&binary, "enabled", &[])),
        "false false (false, true)"
    );
    let binary = f.build(
        &["enabled"],
        &["--no-default-features", "--features", "selected"],
    );
    assert_eq!(
        stdout(&run(&binary, "enabled", &[])),
        "false true (false, true)"
    );
}

#[test]
fn shifts_arguments_in_external_nested_modules_and_macro_arguments() {
    let f = Fixture::new();
    f.package("moduleargs", "mod helper; fn main(){helper::run();}", "");
    f.file(
        "moduleargs/src/helper.rs",
        "mod nested; pub fn run(){nested::run();}",
    );
    f.file("moduleargs/src/helper/nested.rs", "pub fn run(){let args:Vec<_>=std::env::args().skip(1).collect(); println!(\"{:?}\",args); println!(\"{:?}\",std::env::args().skip(1).collect::<Vec<_>>());}");
    let binary = f.build(&["moduleargs"], &[]);
    let output = run(&binary, "moduleargs", &["payload"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "[\"payload\"]\n[\"payload\"]");
    #[cfg(unix)]
    {
        // argv[0] must equal the actual registered command for symlink dispatch.
        let links = f.path().join("links");
        fs::create_dir(&links).unwrap();
        let command = links.join("moduleargs");
        std::os::unix::fs::symlink(&binary, &command).unwrap();
        let output = checked(Command::new(&command).arg("payload"));
        assert_eq!(stdout(&output), "[\"payload\"]\n[\"payload\"]");
    }
}

#[test]
fn preserves_exitcode_results_aliases_and_custom_termination() {
    let f = Fixture::new();
    f.package(
        "exitcode",
        "fn main()->std::process::ExitCode{std::process::ExitCode::FAILURE}",
        "",
    );
    f.package("customexit", "struct Custom; impl std::process::Termination for Custom {fn report(self)->std::process::ExitCode{std::process::ExitCode::from(37)}} fn main()->Custom{Custom}", "");
    f.package("resultcode", "fn main()->Result<std::process::ExitCode,std::io::Error>{Ok(std::process::ExitCode::from(23))}", "");
    f.package("aliasresult", "type Outcome=Result<(),std::io::Error>; fn main()->Outcome{Err(std::io::Error::other(\"expected\"))}", "");
    let binary = f.build(
        &["exitcode", "customexit", "resultcode", "aliasresult"],
        &[],
    );
    for (command, code) in [
        ("exitcode", 1),
        ("customexit", 37),
        ("resultcode", 23),
        ("aliasresult", 1),
    ] {
        assert_eq!(
            run(&binary, command, &[]).status.code(),
            Some(code),
            "{command}"
        );
    }
}

#[test]
fn preserves_current_thread_tokio_main_with_typed_result() {
    let f = Fixture::new();
    f.package("tokioflavor", "#[tokio::main(flavor=\"current_thread\")] async fn main()->std::io::Result<std::process::ExitCode>{tokio::task::yield_now().await;Ok(std::process::ExitCode::from(19))}", "[dependencies]\ntokio={version='1',features=['rt','macros']}\n");
    let binary = f.build(&["tokioflavor"], &[]);
    assert_eq!(run(&binary, "tokioflavor", &[]).status.code(), Some(19));
}

#[test]
fn preserves_build_scripts_and_their_feature_configuration() {
    let f = Fixture::new();
    f.package("generated", "include!(concat!(env!(\"OUT_DIR\"),\"/generated.rs\")); fn main(){println!(\"{}\",ENABLED);}", "[features]\ndefault=['enabled']\nenabled=[]");
    f.file("generated/build.rs", r#"fn main(){let enabled=std::env::var_os("CARGO_FEATURE_ENABLED").is_some();std::fs::write(std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("generated.rs"),format!("const ENABLED:bool={enabled};")).unwrap();}"#);
    let binary = f.build(&["generated"], &[]);
    assert_eq!(stdout(&run(&binary, "generated", &[])), "true");
    let binary = f.build(&["generated"], &["--no-default-features"]);
    assert_eq!(stdout(&run(&binary, "generated", &[])), "false");
}

#[test]
fn rewrites_only_verified_clap_parsers_in_external_modules() {
    let f = Fixture::new();
    f.package("parsecalls", "use clap::Parser; mod options; mod unrelated; use options::Cli as Args; fn main(){let _=unrelated::Cli::parse(); let args=Args::parse(); println!(\"{}\",args.value);}", "[dependencies]\nclap={version='4',features=['derive']}\n");
    f.file(
        "parsecalls/src/options.rs",
        "use clap::Parser; #[derive(Parser)] pub struct Cli{pub value:String}",
    );
    f.file(
        "parsecalls/src/unrelated.rs",
        "pub struct Cli; impl Cli{pub fn parse()->Self{Self}}",
    );
    let binary = f.build(&["parsecalls"], &[]);
    let output = run(&binary, "parsecalls", &["payload"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout(&output), "payload");
}

fn git(path: &Path, args: &[&str]) {
    checked(Command::new("git").arg("-C").arg(path).args(args));
}
fn commit_repo(path: &Path) {
    git(path, &["init", "--quiet"]);
    git(path, &["add", "."]);
    git(
        path,
        &[
            "-c",
            "user.name=rustbb-test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
}

#[test]
fn preserves_inherited_git_dependency_identity_for_workspace_library_and_binary() {
    let f = Fixture::new();
    f.file("policy/Cargo.toml", "[package]\nname='rustbb-test-unpublished-policy'\nversion='0.1.0'\nedition='2021'\n[features]\ndefault=['extra']\nextra=[]\nrequired=[]");
    f.file(
        "policy/src/lib.rs",
        "#[cfg(feature=\"required\")] pub fn check()->bool{!cfg!(feature=\"extra\")}",
    );
    let policy = f.path().join("policy");
    commit_repo(&policy);
    let url = format!("file://{}", policy.display());
    f.file("workspace/Cargo.toml", &format!("[workspace]\nmembers=['app']\nresolver='2'\n[workspace.package]\nversion='1.2.3'\nedition='2021'\n[workspace.dependencies]\npolicy={{git={url:?},package='rustbb-test-unpublished-policy',default-features=false,features=['required']}}\n"));
    f.file("workspace/app/Cargo.toml", "[package]\nname='policyapp'\nversion.workspace=true\nedition.workspace=true\n[dependencies]\npolicy.workspace=true\n");
    f.file(
        "workspace/app/src/lib.rs",
        "pub fn check()->bool{policy::check()}",
    );
    f.file(
        "workspace/app/src/main.rs",
        "fn main(){println!(\"{}\",policyapp::check());}",
    );
    // Only a local file:// Git fetch; no registry/network dependencies in this fixture.
    checked(
        Command::new("cargo")
            .args(["fetch", "--manifest-path"])
            .arg(f.path().join("workspace/Cargo.toml"))
            .env("CARGO_NET_OFFLINE", "false"),
    );
    let binary = f.build(&["workspace"], &[]);
    assert_eq!(stdout(&run(&binary, "policyapp", &[])), "true");
}

#[test]
fn ignores_poisoned_legacy_git_cache_and_fails_closed() {
    let f = Fixture::new();
    let url = format!("file://{}/does-not-exist.git", f.path().display());
    let hash = url
        .bytes()
        .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64));
    let legacy = format!("cache/git-{hash:016x}");
    f.file(
        &format!("{legacy}/Cargo.toml"),
        "[package]\nname='injected'\nversion='0.1.0'\nedition='2021'",
    );
    f.file(&format!("{legacy}/src/lib.rs"), "pub fn injected(){}");
    f.file(&format!("{legacy}/src/main.rs"), "fn main(){}");
    let marker = f.path().join("executed");
    f.file(
        &format!("{legacy}/build.rs"),
        &format!(
            "fn main(){{std::fs::write({:?},\"executed\").unwrap();}}",
            marker
        ),
    );
    commit_repo(&f.path().join(&legacy));
    // Match production's private cache permissions, so rejection cannot mask the test.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(f.path().join("cache"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_rustbb"))
        .arg("build")
        .arg(format!("git:{url}"))
        .arg("--cache-dir")
        .arg(f.path().join("cache"))
        .args(["-o", "combined"])
        .current_dir(f.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Failed to clone"));
    assert!(!marker.exists());
    assert!(!f.path().join("combined").exists());
}

#[test]
fn fetches_binary_only_package_from_a_local_registry_by_resolved_version() {
    let f = Fixture::new();
    // A deterministic .crate tar.gz containing Cargo.toml and src/main.rs for
    // binonly_fixture 1.2.3 (no lib target). Its main prints args().skip(1).
    const ARCHIVE: &str = "1f8b08000000000002ffedd54d4fc3201c0670cefd1495539b28eb9b33c16d17bf83176316dab1858d42036cd12cfbee624fba78344d8ccfeff2e72581033cd02a638d7e5f6fd55b383a7957b28ad5b327e1769605db6bf20b8a68de34638dae6b517e698fe3657ddfd4242dc8048e3e0817b727ffd3cb20ba83d8c9d7c4885e2e69fbfd3ed0e4249d57d62ce978336822372a8cfdaaa84a9a10f8d3da1ff3ef5d37eb8532ccf929f25f55d7f97f8805f99fc0d6a49f079de5e7c12913b4b9c9e8f9426f7dd8702ecd89f3f815f82c67fea086accc5967b5965de07cf12cbbc57ab58a737b1b57a029cdf3c70b5e0400000000000000000000000000000080497d008cd7f38600280000";
    let archive: Vec<_> = (0..ARCHIVE.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&ARCHIVE[i..i + 2], 16).unwrap())
        .collect();
    f.file("downloads/.keep", "");
    fs::write(
        f.path().join("downloads/binonly_fixture-1.2.3.crate"),
        archive,
    )
    .unwrap();
    let dl = format!(
        "file://{}/downloads/{{crate}}-{{version}}.crate",
        f.path().display()
    );
    f.file(
        "index/config.json",
        &serde_json::json!({"dl": dl}).to_string(),
    );
    f.file("index/bi/no/binonly_fixture", &format!("{}\n", serde_json::json!({
        "name": "binonly_fixture", "vers": "1.2.3", "deps": [], "features": {}, "yanked": false,
        "cksum": "1bb1c66b42a807ffe930d259f7f4e53c0bdb17908d43f257e2644382ad4dce79"
    })));
    commit_repo(&f.path().join("index"));
    let index = format!("file://{}/index", f.path().display());
    f.file("cargo-home/config.toml", &format!("[source.crates-io]\nreplace-with='local-test'\n[source.local-test]\nregistry={index:?}\n"));
    // All registry and download URLs are file://; this exercises actual Cargo
    // resolution/vendoring without depending on crates.io or the user's cache.
    checked(
        Command::new(env!("CARGO_BIN_EXE_rustbb"))
            .args([
                "build",
                "binonly_fixture@1.2",
                "-o",
                "combined",
                "--cache-dir",
            ])
            .arg(f.path().join("cache"))
            .current_dir(f.path())
            .env("CARGO_HOME", f.path().join("cargo-home"))
            .env("CARGO_NET_OFFLINE", "false"),
    );
    let output = run(&f.path().join("combined"), "binonly_fixture", &["payload"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "payload");
}

#[test]
fn registry_semver_requirement_reaches_the_resolved_package() {
    // clap is already in the tool's dependency cache. Cargo versions differ in
    // whether clap ships a fixture binary; either discovery result is acceptable.
    let output = Command::new(env!("CARGO_BIN_EXE_rustbb"))
        .args(["analyze", "clap@4"])
        .env("CARGO_NET_OFFLINE", "true")
        .output()
        .unwrap();
    if output.status.success() {
        assert!(
            stdout(&output).contains("Crate: clap"),
            "{}",
            stdout(&output)
        );
        assert!(stdout(&output).contains("/clap-4."), "{}", stdout(&output));
    } else {
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("no binary target"), "{error}");
    }
}
