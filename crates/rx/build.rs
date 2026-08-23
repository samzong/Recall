use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo"),
    );
    let root_manifest = manifest_dir.join("../..").join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", root_manifest.display());

    let contents = fs::read_to_string(&root_manifest)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", root_manifest.display()));
    let manifest: toml::Value =
        toml::from_str(&contents).expect("failed to parse workspace Cargo.toml");
    let version = manifest
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
        .expect("workspace Cargo.toml is missing package.version");
    println!("cargo:rustc-env=RX_RELEASE_VERSION={version}");
}
