//! In-memory provider and host with the full contract, for tests and dry runs.

mod host;
mod provider;

pub use host::FakeHost;
pub use provider::FakeProvider;
