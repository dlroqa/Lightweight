//! Record the target triple this crate is compiled for.
//!
//! Release archives are named by Rust target triple, and the triple is only
//! known to Cargo at build time, so it is passed through rather than rebuilt
//! from `cfg` values that cannot distinguish every triple.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=RELEASE_UPDATE_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
