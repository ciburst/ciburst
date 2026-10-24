//! Core of ciburst: configuration, dispatcher, GitLab client, provisioning and the provider
//! and host contracts. Providers and hosts live in their own crates and implement
//! [`types::Provider`] and [`types::Host`].

pub mod check;
pub mod config;
pub mod dispatch;
pub mod error;
pub mod fake;
pub mod gitlab;
pub mod http;
pub mod log;
pub mod plan;
pub mod provision;
pub mod store;
pub mod types;

pub mod testing;

pub use error::{Error, Result};
