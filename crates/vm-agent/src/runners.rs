//! The `config.toml` of the VM's one `gitlab-runner run` (M6.3 rule R5, D59): `concurrent` is
//! the VM's slots, one `[[runners]]` entry for the pool runner, one per member runner of the
//! last report answer and, while spill is allowed, one for the spill runner (D54).

use std::collections::BTreeSet;
use std::fmt::Write;

use ciburst_core::types::VmRunner;

use crate::config::Config;

/// Seconds between job requests of an idle runner entry.
pub const CHECK_INTERVAL_S: u32 = 3;

/// A rendered `config.toml` and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerConfig {
    pub content: String,
    /// `[[runners]]` entries.
    pub runners: usize,
    /// Whether the spill runner's entry is among them.
    pub spill: bool,
}

/// One entry per distinct token: the pool runner first, the members by project id, the spill
/// runner last, so the same runners always render the same text.
pub fn render(config: &Config, members: &[VmRunner], spill: bool) -> RunnerConfig {
    let spill_token = config.spill_token.as_deref().filter(|_| spill);
    let mut seen: BTreeSet<&str> = BTreeSet::from([config.runner_token.as_str()]);
    if let Some(token) = spill_token {
        seen.insert(token);
    }
    let mut sorted: Vec<&VmRunner> = members.iter().collect();
    sorted.sort_by_key(|member| member.project_id);

    let mut entries: Vec<(String, &str)> = vec![("pool".to_string(), &config.runner_token)];
    for member in sorted {
        if !member.token.is_empty() && seen.insert(&member.token) {
            entries.push((format!("member-{}", member.project_id), &member.token));
        }
    }
    if let Some(token) = spill_token {
        entries.push(("spill".to_string(), token));
    }

    let mut content = String::from("# Written by ciburst-vm-agent.\n");
    let _ = writeln!(content, "concurrent = {}", config.slots);
    let _ = writeln!(content, "check_interval = {CHECK_INTERVAL_S}");
    for (name, token) in &entries {
        entry(&mut content, config, name, token);
    }
    RunnerConfig {
        content,
        runners: entries.len(),
        spill: spill_token.is_some(),
    }
}

fn entry(out: &mut String, config: &Config, name: &str, token: &str) {
    let _ = writeln!(out, "\n[[runners]]");
    let _ = writeln!(out, "name = {}", quote(name));
    let _ = writeln!(out, "url = {}", quote(&config.server_url));
    let _ = writeln!(out, "token = {}", quote(token));
    let _ = writeln!(out, "executor = \"docker\"");
    if config.cache.is_some() {
        // D44: cache archives in parallel with the fastest compression; the bucket is in the
        // VM's region, so bytes are cheap and archiving was the slow part.
        let _ = writeln!(
            out,
            "environment = [\"FF_USE_FASTZIP=true\", \"CACHE_COMPRESSION_LEVEL=fastest\"]"
        );
    }
    let _ = writeln!(out, "[runners.docker]");
    let _ = writeln!(out, "image = {}", quote(&config.default_image));
    let _ = writeln!(out, "privileged = true");
    // Docker-in-Docker with TLS needs the client certificates shared by service and job.
    let _ = writeln!(out, "volumes = [\"/cache\", \"/certs/client\"]");
    if let Some(cache) = &config.cache {
        let _ = writeln!(out, "[runners.cache]");
        let _ = writeln!(out, "Type = \"s3\"");
        let _ = writeln!(out, "Shared = {}", cache.shared);
        let _ = writeln!(out, "[runners.cache.s3]");
        for (key, value) in [
            ("ServerAddress", &cache.server_address),
            ("BucketName", &cache.bucket_name),
            ("BucketLocation", &cache.bucket_location),
            ("AccessKey", &cache.access_key),
            ("SecretKey", &cache.secret_key),
        ] {
            let _ = writeln!(out, "{key} = {}", quote(value));
        }
    }
}

/// A TOML basic string.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::sample;

    fn member(project_id: u64, token: &str) -> VmRunner {
        VmRunner {
            project_id,
            token: token.to_string(),
        }
    }

    fn entry_text(name: &str, token: &str) -> String {
        format!(
            "
[[runners]]
name = \"{name}\"
url = \"https://gitlab.example.invalid\"
token = \"{token}\"
executor = \"docker\"
environment = [\"FF_USE_FASTZIP=true\", \"CACHE_COMPRESSION_LEVEL=fastest\"]
[runners.docker]
image = \"registry.example.invalid/ciburst/node:1\"
privileged = true
volumes = [\"/cache\", \"/certs/client\"]
[runners.cache]
Type = \"s3\"
Shared = true
[runners.cache.s3]
ServerAddress = \"s3.fr-par.scw.cloud\"
BucketName = \"ci-cache-bucket\"
BucketLocation = \"fr-par\"
AccessKey = \"SCWACCESSKEYSAMPLE\"
SecretKey = \"cache-secret-key\"
"
        )
    }

    const HEAD: &str = "# Written by ciburst-vm-agent.\nconcurrent = 2\ncheck_interval = 3\n";

    #[test]
    fn without_members_only_the_pool_runner() {
        let rendered = render(&sample(), &[], false);
        assert_eq!(
            rendered.content,
            format!("{HEAD}{}", entry_text("pool", "glrt-runner-secret"))
        );
        assert_eq!(rendered.runners, 1);
        assert!(!rendered.spill);
    }

    #[test]
    fn one_member_gets_its_own_entry() {
        let rendered = render(&sample(), &[member(42, "glrt-member-42")], false);
        assert_eq!(
            rendered.content,
            format!(
                "{HEAD}{}{}",
                entry_text("pool", "glrt-runner-secret"),
                entry_text("member-42", "glrt-member-42")
            )
        );
        assert_eq!(rendered.runners, 2);
    }

    #[test]
    fn three_members_sorted_by_project_after_the_pool() {
        let members = [
            member(30, "glrt-member-30"),
            member(10, "glrt-member-10"),
            member(20, "glrt-member-20"),
        ];
        let rendered = render(&sample(), &members, false);
        assert_eq!(
            rendered.content,
            format!(
                "{HEAD}{}{}{}{}",
                entry_text("pool", "glrt-runner-secret"),
                entry_text("member-10", "glrt-member-10"),
                entry_text("member-20", "glrt-member-20"),
                entry_text("member-30", "glrt-member-30")
            )
        );
        assert_eq!(rendered.runners, 4);
        let mut reversed = members.to_vec();
        reversed.reverse();
        assert_eq!(render(&sample(), &reversed, false), rendered);
    }

    #[test]
    fn the_spill_entry_comes_last_while_allowed() {
        let members = [member(7, "glrt-member-7")];
        let rendered = render(&sample(), &members, true);
        assert_eq!(
            rendered.content,
            format!(
                "{HEAD}{}{}{}",
                entry_text("pool", "glrt-runner-secret"),
                entry_text("member-7", "glrt-member-7"),
                entry_text("spill", "glrt-spill-secret")
            )
        );
        assert_eq!(rendered.runners, 3);
        assert!(rendered.spill);

        let mut config = sample();
        config.spill_token = None;
        let rendered = render(&config, &members, true);
        assert_eq!(rendered.runners, 2);
        assert!(!rendered.spill);
        assert!(!rendered.content.contains("name = \"spill\""));
    }

    #[test]
    fn duplicate_and_empty_tokens_get_no_second_entry() {
        let members = [
            member(1, "glrt-runner-secret"),
            member(2, "glrt-spill-secret"),
            member(3, "glrt-member-3"),
            member(4, "glrt-member-3"),
            member(5, ""),
        ];
        let rendered = render(&sample(), &members, true);
        assert_eq!(rendered.runners, 3);
        assert_eq!(rendered.content.matches("[[runners]]").count(), 3);
        assert!(rendered.content.contains("name = \"member-3\""));
        assert!(!rendered.content.contains("member-4"));
        // Without spill, the spill runner's token may serve as a member's.
        let rendered = render(&sample(), &members, false);
        assert_eq!(rendered.runners, 3);
        assert!(rendered.content.contains("name = \"member-2\""));
    }

    #[test]
    fn without_a_cache_no_cache_settings_or_env() {
        let mut config = sample();
        config.cache = None;
        config.slots = 3;
        let rendered = render(&config, &[], false);
        assert_eq!(
            rendered.content,
            "# Written by ciburst-vm-agent.\nconcurrent = 3\ncheck_interval = 3\n
[[runners]]
name = \"pool\"
url = \"https://gitlab.example.invalid\"
token = \"glrt-runner-secret\"
executor = \"docker\"
[runners.docker]
image = \"registry.example.invalid/ciburst/node:1\"
privileged = true
volumes = [\"/cache\", \"/certs/client\"]
"
        );
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(quote("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
    }
}
