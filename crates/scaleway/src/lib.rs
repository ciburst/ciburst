//! Scaleway implementation of the ciburst ports.
//!
//! `provider::ScalewayJobs` runs jobs on Serverless Jobs, `host::ScalewayFunction` deploys the
//! dispatcher as a Serverless Function. Both talk to `https://api.scaleway.com` through
//! `client::ScalewayClient`, which sits on the core's `HttpClient` seam so tests run against the
//! in-memory fakes in `testing`.

pub mod client;
pub mod host;
pub mod provider;
pub mod testing;
