//! In-memory fakes of the Scaleway APIs behind `ciburst_core::http::HttpClient`, with the real
//! request and response shapes. One file per API; the provider tests compose
//! `account_api`, `iam_api`, `jobs_api` and `s3_api`, the host tests use `functions_api`.

pub mod account_api;
pub mod functions_api;
pub mod iam_api;
pub mod jobs_api;
pub mod s3_api;
pub mod scaleway_fake;
