//! An in-memory fake of the Hetzner Cloud API behind `ciburst_core::http::HttpClient`, with the
//! real request and response shapes, and the label selector language it filters with.

pub mod hcloud_api;
pub mod selector;
