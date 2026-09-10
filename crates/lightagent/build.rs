use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=../../CHANGELOG.md");

    let version = env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let changelog = fs::read_to_string(manifest.join("../../CHANGELOG.md")).unwrap_or_default();
    let prefix = format!("## [{version}] - ");
    let release_date = changelog
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::trim)
        .filter(|date| !date.is_empty())
        .unwrap_or("unreleased");

    println!("cargo:rustc-env=LIGHTAGENT_RELEASE_DATE={release_date}");
}
