//! VM capability tokens and the agent's wire format, spec section 6.1.
//!
//! A token is the hex HMAC-SHA256 of the run id under the dispatcher's VM secret. The VM holds
//! only its own token, so it can report its own state and end itself, nothing else.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// Header carrying the run id on `/vm` and `/terminate`.
pub const HEADER_RUN: &str = "X-Ciburst-Run";
/// Header carrying the capability token on `/vm` and `/terminate`.
pub const HEADER_TOKEN: &str = "X-Ciburst-Token";
/// Path of the state report, body [`crate::types::VmReport`] as JSON.
pub const PATH_REPORT: &str = "/vm";
/// Path the VM posts to after its last slot exited; empty body.
pub const PATH_TERMINATE: &str = "/terminate";
/// Body key of the cron trigger's arguments: `{"ciburst_sweep": "<hook token>"}`.
pub const SWEEP_ARG: &str = "ciburst_sweep";

/// Minute of a paid hour from which an idle VM drains.
pub const DRAIN_MINUTE: u64 = 55;
/// Seconds an idle VM keeps polling after reporting `draining`.
pub const DRAIN_GRACE_S: u64 = 30;
/// Seconds between unconditional reports.
pub const HEARTBEAT_S: u64 = 600;
/// Sweep limits in seconds.
pub const SWEEP_STARTING_S: u64 = 600;
pub const SWEEP_DRAINING_S: u64 = 900;
pub const SWEEP_SILENT_S: u64 = 1800;
pub const SWEEP_MAX_AGE_S: u64 = 86_400;

pub fn token(secret: &str, run_id: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(run_id.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn verify(secret: &str, run_id: &str, given: &str) -> bool {
    if secret.is_empty() || run_id.is_empty() {
        return false;
    }
    let expected = token(secret, run_id);
    expected.len() == given.len() && bool::from(expected.as_bytes().ct_eq(given.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_bind_secret_and_run() {
        let a = token("secret", "run-1");
        assert_eq!(a.len(), 64);
        assert!(verify("secret", "run-1", &a));
        assert!(!verify("secret", "run-2", &a));
        assert!(!verify("other", "run-1", &a));
        assert!(!verify("", "run-1", &token("", "run-1")));
        assert!(!verify("secret", "run-1", &a[..63]));
    }
}
