//! Core of ciburst: configuration, dispatcher, GitLab client, provisioning and the provider
//! and host contracts. Providers and hosts live in their own crates and implement
//! [`types::Provider`] and [`types::Host`].

pub mod check;
pub mod cloud_init;
pub mod composite;
pub mod config;
pub mod dispatch;
pub mod error;
pub mod fake;
pub mod gitlab;
pub mod http;
pub mod lane;
pub mod ledger;
pub mod log;
pub mod pipeline;
pub mod plan;
pub mod provision;
pub mod store;
pub mod time;
pub mod types;
pub mod vm;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use error::{Error, Result};
