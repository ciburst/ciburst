//! Serverless lanes (D45): a lane run starts from the job's own image, with `lane-bootstrap.sh`
//! as its command. The script installs missing tools, fetches the pinned gitlab-runner from the
//! deployment's bucket and runs `run-single` with the shell executor.

/// The bootstrap, shipped base64 encoded in [`SCRIPT_KEY`] of the run's environment.
pub const BOOTSTRAP: &str = include_str!("lane-bootstrap.sh");

/// The run's command. Scaleway splits a job command at spaces and ignores quotes, so the
/// command holds none; the shell turns `${IFS}` back into spaces.
pub const COMMAND: &str = "/bin/sh -c eval${IFS}\"$(echo${IFS}$CIBURST_LANE|base64${IFS}-d)\"";

/// Environment key with the base64 encoded [`BOOTSTRAP`].
pub const SCRIPT_KEY: &str = "CIBURST_LANE";
/// Environment key with the public URL of the runner object in the deployment's bucket.
pub const RUNNER_URL_KEY: &str = "CIBURST_RUNNER_URL";
/// Environment key with the runner's SHA-256 (`cloud_init::RUNNER_SHA256`).
pub const RUNNER_SHA256_KEY: &str = "CIBURST_RUNNER_SHA256";
/// Object key prefix of the runner binary in the deployment's bucket.
pub const RUNNER_PREFIX: &str = "ciburst/runner/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_has_no_space_inside_its_shell_argument() {
        let parts: Vec<&str> = COMMAND.split(' ').collect();
        assert_eq!(parts.len(), 3, "{COMMAND}");
        assert_eq!(&parts[..2], ["/bin/sh", "-c"]);
        assert!(parts[2].contains(SCRIPT_KEY));
    }

    #[test]
    fn the_bootstrap_reads_the_runner_keys() {
        for key in [
            RUNNER_URL_KEY,
            RUNNER_SHA256_KEY,
            "RUNNER_TOKEN",
            "CI_SERVER_URL",
        ] {
            assert!(BOOTSTRAP.contains(key), "{key}");
        }
        assert!(BOOTSTRAP.starts_with("#!/bin/sh\n"));
    }
}
