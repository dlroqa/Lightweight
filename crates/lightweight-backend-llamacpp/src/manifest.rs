//! Which engine build we run, and where to get it.
//!
//! The engine is pinned to one llama.cpp build and verified against a digest
//! recorded here. Two reasons that matters more than it might seem:
//!
//! * **Reproducibility.** The gateway re-emits its own response shapes rather
//!   than forwarding the engine's, so an upstream change breaks one of our
//!   adapter tests rather than breaking clients — but only if everyone is
//!   running the build the tests were written against.
//! * **Integrity.** The binary is downloaded over the network and then executed.
//!   A digest that is checked before it ever runs is the difference between a
//!   download and an install.
//!
//! Digests come from the GitHub release API's `digest` field for the pinned
//! tag. The `linux-x64` entry was independently confirmed by `sha256sum` on a
//! separately fetched copy.
//!
//! ## Why prebuilt binaries at all
//!
//! Because they work here, and building does not. llama.cpp's top-level
//! `Makefile` is a hard `$(error)` stub, so a source build needs CMake, which
//! cannot be installed without sudo. And the official Linux and Windows CPU
//! artifacts are built with `GGML_BACKEND_DL=ON GGML_CPU_ALL_VARIANTS=ON`, so
//! they ship every CPU variant as a separate shared object and select one at
//! runtime by score. Measured on the development machine, an Intel Pentium
//! Silver with no AVX at all: `sse42` scores 5, `x64` scores 1, and every
//! AVX-and-above variant scores 0. One artifact is therefore both correct here
//! and optimal on a modern machine.

use serde::{Deserialize, Serialize};

/// The pinned llama.cpp build.
///
/// Moving this means re-recording every digest below and re-running the
/// contract tests. It is deliberately a single place.
pub const PINNED_BUILD: &str = "b10590";

/// How an artifact is packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    TarGz,
    Zip,
}

/// One platform's engine artifact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeArtifact {
    /// `std::env::consts::OS` value this is for.
    pub os: &'static str,
    /// `std::env::consts::ARCH` value this is for.
    pub arch: &'static str,
    /// Release asset file name.
    pub asset: &'static str,
    pub format: ArchiveFormat,
    /// Lowercase hex sha256 of the asset.
    pub sha256: &'static str,
    /// Asset size in bytes, used to show progress and to check free disk
    /// before starting.
    pub size: u64,
}

impl RuntimeArtifact {
    /// Where to download this asset from.
    pub fn url(&self) -> String {
        format!(
            "https://github.com/ggml-org/llama.cpp/releases/download/{PINNED_BUILD}/{}",
            self.asset
        )
    }

    /// Directory name the artifact is extracted into.
    ///
    /// Includes the build id, so upgrading installs alongside rather than over
    /// the top of what is already there. A half-overwritten engine directory
    /// would be a genuinely nasty failure to diagnose.
    pub fn install_dir_name(&self) -> String {
        format!("llama.cpp-{PINNED_BUILD}-{}-{}", self.os, self.arch)
    }
}

/// Every platform we ship an engine for.
///
/// CPU artifacts only. Spec section 2 requires the product to run without CUDA,
/// ROCm, Metal, Vulkan or DirectML, and section 29 says GPU backends stay
/// unimplemented for now, so the CUDA and Vulkan assets in the same release are
/// deliberately not listed.
pub const ARTIFACTS: &[RuntimeArtifact] = &[
    RuntimeArtifact {
        os: "linux",
        arch: "x86_64",
        asset: "llama-b10590-bin-ubuntu-x64.tar.gz",
        format: ArchiveFormat::TarGz,
        sha256: "4efbac3e8a647c49cc4856248fa295937b94921e31cdb2c964bf8c5772473559",
        size: 16_369_239,
    },
    RuntimeArtifact {
        os: "linux",
        arch: "aarch64",
        asset: "llama-b10590-bin-ubuntu-arm64.tar.gz",
        format: ArchiveFormat::TarGz,
        sha256: "12999190e14133086dd4a6be57ab23484edb29b79e1d15677b3fb09d78cf3e2f",
        size: 13_115_141,
    },
    RuntimeArtifact {
        os: "macos",
        arch: "aarch64",
        asset: "llama-b10590-bin-macos-arm64.tar.gz",
        format: ArchiveFormat::TarGz,
        sha256: "6bd011f97a27eb27e296fa17867948d97988857ddde98159fca925e2d73a1362",
        size: 10_805_509,
    },
    RuntimeArtifact {
        os: "macos",
        arch: "x86_64",
        asset: "llama-b10590-bin-macos-x64.tar.gz",
        format: ArchiveFormat::TarGz,
        sha256: "ba08608c77cd28f81cd27a98c4829b2513eaf053b1168bf32ca63ffc991f88a3",
        size: 11_094_133,
    },
    RuntimeArtifact {
        os: "windows",
        arch: "x86_64",
        asset: "llama-b10590-bin-win-cpu-x64.zip",
        format: ArchiveFormat::Zip,
        sha256: "98d942240a61a5c628c16d7951c041095e63a741916c74110e785129e10c2eaa",
        size: 18_132_388,
    },
];

/// The artifact for the machine we are running on.
pub fn for_this_platform() -> Option<&'static RuntimeArtifact> {
    for_platform(std::env::consts::OS, std::env::consts::ARCH)
}

/// The artifact for a named platform.
pub fn for_platform(os: &str, arch: &str) -> Option<&'static RuntimeArtifact> {
    ARTIFACTS
        .iter()
        .find(|artifact| artifact.os == os && artifact.arch == arch)
}

/// Name of the server executable inside an extracted artifact.
pub const fn server_executable() -> &'static str {
    if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_digest_is_a_well_formed_sha256() {
        // A malformed digest would never match, turning every install into an
        // integrity failure that looks like a corrupt download.
        for artifact in ARTIFACTS {
            assert_eq!(
                artifact.sha256.len(),
                64,
                "{} has a {}-character digest",
                artifact.asset,
                artifact.sha256.len()
            );
            assert!(
                artifact
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                "{} digest is not lowercase hex",
                artifact.asset
            );
        }
    }

    #[test]
    fn the_linux_x64_digest_matches_the_independently_verified_one() {
        // Confirmed with `sha256sum` against a separately downloaded copy, not
        // only against the release API that also supplied it.
        let artifact = for_platform("linux", "x86_64").expect("linux x64 is supported");
        assert_eq!(
            artifact.sha256,
            "4efbac3e8a647c49cc4856248fa295937b94921e31cdb2c964bf8c5772473559"
        );
        assert_eq!(artifact.size, 16_369_239);
    }

    #[test]
    fn every_asset_name_carries_the_pinned_build() {
        // Guards the most likely mistake when moving the pin: updating the
        // constant and leaving an asset name behind.
        for artifact in ARTIFACTS {
            assert!(
                artifact.asset.contains(PINNED_BUILD),
                "{} does not match the pinned build {PINNED_BUILD}",
                artifact.asset
            );
        }
    }

    #[test]
    fn no_gpu_artifact_is_listed() {
        // Section 2: the product must not require CUDA, ROCm, Vulkan or SYCL.
        // The same release publishes those assets; none may appear here.
        for artifact in ARTIFACTS {
            let name = artifact.asset;
            for forbidden in ["cuda", "rocm", "vulkan", "sycl", "openvino"] {
                assert!(
                    !name.contains(forbidden),
                    "{name} is a GPU build and must not be shipped"
                );
            }
        }
    }

    #[test]
    fn platform_lookup_is_exact() {
        assert!(for_platform("linux", "x86_64").is_some());
        assert!(for_platform("macos", "aarch64").is_some());
        assert!(for_platform("windows", "x86_64").is_some());
        // Not guessing a near match is the point: silently handing a Linux
        // binary to a FreeBSD user would fail far more confusingly.
        assert!(for_platform("freebsd", "x86_64").is_none());
        assert!(for_platform("linux", "riscv64").is_none());
    }

    #[test]
    fn this_machine_has_an_artifact() {
        // The development and CI platform must be covered, or nothing runs.
        assert!(
            for_this_platform().is_some(),
            "no engine artifact for {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    }

    #[test]
    fn install_directories_are_unique_per_platform_and_build() {
        // Two platforms sharing a directory would let one overwrite the other
        // on a shared volume.
        let mut seen = std::collections::BTreeSet::new();
        for artifact in ARTIFACTS {
            assert!(
                seen.insert(artifact.install_dir_name()),
                "duplicate install directory {}",
                artifact.install_dir_name()
            );
            assert!(artifact.install_dir_name().contains(PINNED_BUILD));
        }
    }

    #[test]
    fn download_urls_point_at_the_pinned_release() {
        let artifact = for_platform("linux", "x86_64").expect("linux x64");
        let url = artifact.url();
        assert!(url.starts_with("https://github.com/ggml-org/llama.cpp/releases/download/"));
        assert!(url.contains(PINNED_BUILD));
        assert!(url.ends_with(artifact.asset));
    }

    #[test]
    fn windows_uses_zip_and_everything_else_uses_tar_gz() {
        for artifact in ARTIFACTS {
            let expected = if artifact.os == "windows" {
                ArchiveFormat::Zip
            } else {
                ArchiveFormat::TarGz
            };
            assert_eq!(artifact.format, expected, "{}", artifact.asset);
        }
    }
}
