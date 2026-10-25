//! In-memory fakes of the Scaleway APIs behind `ciburst_core::http::HttpClient`, with the real
//! request and response shapes. One file per API; the provider tests compose
//! `account_api`, `iam_api`, `jobs_api`, `registry_api` and `s3_api`, the host tests use `functions_api`.

pub mod account_api;
pub mod functions_api;
pub mod iam_api;
pub mod instance_api;
pub mod jobs_api;
pub mod registry_api;
pub mod s3_api;
pub mod scaleway_fake;

use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// A stand-in for the musl `ciburst-vm-agent`: a few bytes behind the ELF magic.
pub fn agent_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("ciburst-vm-agent")
}

/// The SHA-256 of [`agent_fixture`] in hex.
pub fn agent_fixture_sha256() -> String {
    let bytes = std::fs::read(agent_fixture()).expect("agent fixture");
    hex::encode(Sha256::digest(bytes))
}
