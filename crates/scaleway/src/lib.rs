//! Scaleway implementation of the ciburst ports.
//!
//! `provider::Scaleway` runs jobs on Instances and Serverless Jobs, `host::ScalewayFunction`
//! deploys the dispatcher as a Serverless Function, `ledger::ScalewayLedger` keeps burst intents
//! and `records::ScalewayPipelineRecords` the warm start records in the cache bucket. Provider
//! and host talk to `https://api.scaleway.com` through `client::ScalewayClient`, the S3 code to
//! `s3.<region>.scw.cloud`; all of it sits on the core's `HttpClient` seam so tests run against
//! the in-memory fakes in `testing`.

pub mod client;
pub mod host;
pub mod ledger;
pub mod provider;
pub mod records;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
