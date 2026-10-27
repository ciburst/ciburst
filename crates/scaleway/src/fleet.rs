//! The fleet state of D58 (spec 8.1) in the cache bucket, beside the ledger and
//! signed with the same cache key:
//!
//! - `ciburst/fleet/config.json`: `{"config": "<the routing config JSON as given>", "mac":
//!   "<base64 HMAC-SHA256>"}`. The config is plain text, it holds no secret; the MAC binds it
//!   to the fleet key, because every job can write the bucket with the cache key (spec 6.4) and
//!   the config names the images lane runs use.
//! - `ciburst/fleet/members/<project id>.json`: one [`Member`], its webhook secret and runner
//!   tokens sealed with AES-256-GCM, each value `base64(nonce, 12 bytes || ciphertext || tag,
//!   16 bytes)` with a random nonce, and the object key and field as associated data, so a value
//!   moved to another member or field does not open.
//!
//! The prefix `ciburst/fleet/` lies outside every lifecycle rule of the bucket
//! ([`crate::provider::s3::LIFECYCLE_RULES`]), so the state never expires.
//!
//! Both keys come from the function secret `CIBURST_FLEET_KEY`: the AES key is
//! HMAC-SHA256(secret, `ciburst fleet aes-256-gcm`), the MAC key HMAC-SHA256(secret, `ciburst
//! fleet config`). Errors name the object key and the field, never a value.

use futures_util::future::join_all;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use async_trait::async_trait;
use ciburst_core::config::Config;
use ciburst_core::fleet::{FleetStore, Member, MemberLane};
use ciburst_core::http::HttpClient;
use ciburst_core::types::KeyPair;
use ciburst_core::{Error, Result};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::ledger::{MAX_PAGES, is_id};
use crate::provider::bucket_name;
use crate::provider::s3::{S3, base64};

/// Every fleet object lives below this prefix.
pub const FLEET_PREFIX: &str = "ciburst/fleet/";
/// The routing config.
pub const CONFIG_KEY: &str = "ciburst/fleet/config.json";
/// The member records, one per project.
pub const MEMBER_PREFIX: &str = "ciburst/fleet/members/";
/// The only cipher of a member record.
pub const CIPHER: &str = "aes-256-gcm";
/// Shortest `CIBURST_FLEET_KEY` accepted; setup makes 32 random alphanumerics.
pub const MIN_SECRET_LEN: usize = 32;

const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;
const AES_LABEL: &[u8] = b"ciburst fleet aes-256-gcm";
const MAC_LABEL: &[u8] = b"ciburst fleet config";

type HmacSha256 = Hmac<Sha256>;

/// The keys derived from `CIBURST_FLEET_KEY`. Debug output shows no key material.
#[derive(Clone)]
pub struct FleetKey {
    aes: [u8; 32],
    mac: [u8; 32],
}

impl fmt::Debug for FleetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FleetKey(..)")
    }
}

fn derive(secret: &[u8], label: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(label);
    mac.finalize().into_bytes().into()
}

impl FleetKey {
    /// The keys of `secret`, which must hold at least [`MIN_SECRET_LEN`] characters.
    pub fn from_secret(secret: &str) -> Result<FleetKey> {
        if secret.len() < MIN_SECRET_LEN {
            return Err(invalid(format!(
                "fleet key shorter than {MIN_SECRET_LEN} characters"
            )));
        }
        Ok(FleetKey {
            aes: derive(secret.as_bytes(), AES_LABEL),
            mac: derive(secret.as_bytes(), MAC_LABEL),
        })
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(&self.aes).expect("32-byte key")
    }

    /// `base64(nonce || ciphertext || tag)` of `plain`, bound to `aad`.
    pub fn seal(&self, aad: &str, plain: &str) -> Result<String> {
        let mut nonce = [0u8; NONCE_BYTES];
        rand::rng().fill_bytes(&mut nonce);
        let sealed = self
            .cipher()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plain.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| invalid(format!("{aad}: cannot encrypt")))?;
        let mut out = nonce.to_vec();
        out.extend(sealed);
        Ok(base64(&out))
    }

    /// The plain text of a [`FleetKey::seal`] value; an error naming `aad` when the value is
    /// malformed, altered, bound to other data or sealed with another key.
    pub fn open(&self, aad: &str, value: &str) -> Result<String> {
        let failed = || {
            invalid(format!(
                "{aad}: cannot decrypt (wrong fleet key or altered)"
            ))
        };
        let bytes = base64_decode(value).ok_or_else(failed)?;
        if bytes.len() < NONCE_BYTES + TAG_BYTES {
            return Err(failed());
        }
        let (nonce, sealed) = bytes.split_at(NONCE_BYTES);
        let plain = self
            .cipher()
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| failed())?;
        String::from_utf8(plain).map_err(|_| failed())
    }

    fn config_mac(&self, json: &str) -> String {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.mac).expect("32-byte key");
        mac.update(json.as_bytes());
        base64(&mac.finalize().into_bytes())
    }

    fn config_mac_ok(&self, json: &str, found: &str) -> bool {
        let Some(found) = base64_decode(found) else {
            return false;
        };
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.mac).expect("32-byte key");
        mac.update(json.as_bytes());
        mac.verify_slice(&found).is_ok()
    }
}

fn invalid(what: String) -> Error {
    Error::provider(format!("scaleway fleet: {what}"))
}

/// `ciburst/fleet/members/<project id>.json`.
pub fn member_key(project_id: u64) -> String {
    format!("{MEMBER_PREFIX}{project_id}.json")
}

/// The project a member key stands for; `None` for any other key.
pub fn parse_member_key(key: &str) -> Option<u64> {
    let id = key.strip_prefix(MEMBER_PREFIX)?.strip_suffix(".json")?;
    if !is_id(id) {
        return None;
    }
    id.parse().ok()
}

fn hook_aad(key: &str) -> String {
    format!("{key}#hookSecret")
}

fn runner_aad(key: &str, tag: &str) -> String {
    format!("{key}#runners/{tag}")
}

fn lane_aad(key: &str, tag: &str) -> String {
    format!("{key}#lanes/{tag}")
}

#[derive(Serialize, Deserialize)]
struct StoredConfig {
    config: String,
    mac: String,
}

/// A member as stored: [`Member`] with sealed secrets and the cipher named.
#[derive(Serialize, Deserialize)]
struct StoredMember {
    #[serde(rename = "projectId")]
    project_id: u64,
    cipher: String,
    #[serde(rename = "hookSecret")]
    hook_secret: String,
    runners: BTreeMap<String, String>,
    /// The member's lanes with sealed runner tokens (spec 8.1); a record before M6.4 has
    /// none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lanes: Vec<MemberLane>,
    #[serde(default, rename = "ignoreJobs", skip_serializing_if = "Vec::is_empty")]
    ignore_jobs: Vec<String>,
    #[serde(rename = "joinedAt")]
    joined_at: u64,
}

pub struct ScalewayFleetStore {
    s3: S3,
    bucket: String,
    key: FleetKey,
}

impl ScalewayFleetStore {
    pub fn new(
        http: Arc<dyn HttpClient>,
        region: &str,
        bucket: impl Into<String>,
        cache_key: &KeyPair,
        fleet_key: FleetKey,
    ) -> ScalewayFleetStore {
        ScalewayFleetStore {
            s3: S3::with_key(http, region, cache_key),
            bucket: bucket.into(),
            key: fleet_key,
        }
    }

    /// The store in the deployment's cache bucket `<name>-cache-<project id>`.
    pub fn in_cache_bucket(
        http: Arc<dyn HttpClient>,
        config: &Config,
        project_id: &str,
        region: &str,
        cache_key: &KeyPair,
        fleet_key: FleetKey,
    ) -> ScalewayFleetStore {
        ScalewayFleetStore::new(
            http,
            region,
            bucket_name(config, project_id),
            cache_key,
            fleet_key,
        )
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    fn seal_member(&self, member: &Member) -> Result<StoredMember> {
        let key = member_key(member.project_id);
        let runners = member
            .runners
            .iter()
            .map(|(tag, token)| Ok((tag.clone(), self.key.seal(&runner_aad(&key, tag), token)?)))
            .collect::<Result<_>>()?;
        let lanes = member
            .lanes
            .iter()
            .map(|lane| {
                Ok(MemberLane {
                    runner_token: self
                        .key
                        .seal(&lane_aad(&key, &lane.tag), &lane.runner_token)?,
                    ..lane.clone()
                })
            })
            .collect::<Result<_>>()?;
        Ok(StoredMember {
            project_id: member.project_id,
            cipher: CIPHER.to_string(),
            hook_secret: self.key.seal(&hook_aad(&key), &member.hook_secret)?,
            runners,
            lanes,
            ignore_jobs: member.ignore_jobs.clone(),
            joined_at: member.joined_at,
        })
    }

    fn open_member(&self, key: &str, body: &[u8]) -> Result<Member> {
        let stored: StoredMember =
            serde_json::from_slice(body).map_err(|_| invalid(format!("{key}: not a member")))?;
        if stored.cipher != CIPHER {
            return Err(invalid(format!("{key}: cipher is not {CIPHER}")));
        }
        if member_key(stored.project_id) != key {
            return Err(invalid(format!(
                "{key}: holds project {}",
                stored.project_id
            )));
        }
        let runners = stored
            .runners
            .iter()
            .map(|(tag, token)| Ok((tag.clone(), self.key.open(&runner_aad(key, tag), token)?)))
            .collect::<Result<_>>()?;
        let lanes = stored
            .lanes
            .iter()
            .map(|lane| {
                Ok(MemberLane {
                    runner_token: self
                        .key
                        .open(&lane_aad(key, &lane.tag), &lane.runner_token)?,
                    ..lane.clone()
                })
            })
            .collect::<Result<_>>()?;
        Ok(Member {
            project_id: stored.project_id,
            hook_secret: self.key.open(&hook_aad(key), &stored.hook_secret)?,
            runners,
            lanes,
            ignore_jobs: stored.ignore_jobs,
            joined_at: stored.joined_at,
        })
    }
}

#[async_trait]
impl FleetStore for ScalewayFleetStore {
    async fn config(&self) -> Result<Option<String>> {
        let Some(body) = self.s3.get_object(&self.bucket, CONFIG_KEY).await? else {
            return Ok(None);
        };
        let stored: StoredConfig = serde_json::from_slice(&body)
            .map_err(|_| invalid(format!("{CONFIG_KEY}: not a fleet config")))?;
        if !self.key.config_mac_ok(&stored.config, &stored.mac) {
            return Err(invalid(format!(
                "{CONFIG_KEY}: MAC does not match (wrong fleet key or altered)"
            )));
        }
        Ok(Some(stored.config))
    }

    async fn put_config(&self, json: &str) -> Result<()> {
        let stored = StoredConfig {
            config: json.to_string(),
            mac: self.key.config_mac(json),
        };
        let body = serde_json::to_vec(&stored)?;
        self.s3.put_object(&self.bucket, CONFIG_KEY, body).await
    }

    async fn members(&self) -> Result<Vec<Member>> {
        let listed = self
            .s3
            .list_all(&self.bucket, MEMBER_PREFIX, MAX_PAGES)
            .await?;
        if let Some(object) = listed
            .iter()
            .find(|object| parse_member_key(&object.key).is_none())
        {
            return Err(invalid(format!("{}: not a member key", object.key)));
        }
        // All at once: a cold dispatcher reads the fleet before it answers GitLab, which gives
        // up after 10 s (M6.3 drill).
        let bodies = join_all(
            listed
                .iter()
                .map(|object| self.s3.get_object(&self.bucket, &object.key)),
        )
        .await;
        let mut members = Vec::with_capacity(listed.len());
        for (object, body) in listed.iter().zip(bodies) {
            // Deleted between the listing and the read: a member that left.
            let Some(body) = body? else {
                continue;
            };
            members.push(self.open_member(&object.key, &body)?);
        }
        Ok(members)
    }

    async fn put_member(&self, member: &Member) -> Result<()> {
        let stored = self.seal_member(member)?;
        let body = serde_json::to_vec(&stored)?;
        self.s3
            .put_object(&self.bucket, &member_key(member.project_id), body)
            .await
    }

    async fn delete_member(&self, project_id: u64) -> Result<()> {
        // S3 answers 204 for a missing key too.
        self.s3
            .delete_object(&self.bucket, &member_key(project_id))
            .await
    }
}

/// Standard base64 with padding, as [`base64`] writes it; `None` for anything else.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    if text.is_empty() || !text.len().is_multiple_of(4) {
        return None;
    }
    let chunks: Vec<&[u8]> = text.as_bytes().chunks(4).collect();
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for (index, chunk) in chunks.iter().enumerate() {
        let mut n = 0u32;
        let mut padding = 0;
        for &byte in *chunk {
            let value = if byte == b'=' {
                padding += 1;
                0
            } else if padding > 0 {
                return None;
            } else {
                ALPHABET.iter().position(|&c| c == byte)? as u32
            };
            n = (n << 6) | value;
        }
        if padding > 2 || (padding > 0 && index + 1 != chunks.len()) {
            return None;
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - padding]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::s3::LIFECYCLE_RULES;

    const SECRET: &str = "abcdefghijklmnopqrstuvwxyz012345";

    #[test]
    fn member_keys_read_back() {
        assert_eq!(member_key(7), "ciburst/fleet/members/7.json");
        assert_eq!(parse_member_key(&member_key(u64::MAX)), Some(u64::MAX));
        for key in [
            "ciburst/fleet/members/07.json",
            "ciburst/fleet/members/7",
            "ciburst/fleet/members/x.json",
            "ciburst/fleet/members/7.json.bak",
            "ciburst/fleet/members/a/7.json",
            "ciburst/fleet/config.json",
        ] {
            assert_eq!(parse_member_key(key), None, "{key}");
        }
    }

    #[test]
    fn the_fleet_prefix_never_expires() {
        for (id, prefix) in LIFECYCLE_RULES {
            assert!(
                !FLEET_PREFIX.starts_with(prefix) && !prefix.starts_with(FLEET_PREFIX),
                "{id} covers {prefix}"
            );
        }
    }

    #[test]
    fn sealed_values_open_only_with_their_key_and_data() {
        let key = FleetKey::from_secret(SECRET).unwrap();
        let sealed = key.seal("a#hookSecret", "glrt-token").unwrap();
        let again = key.seal("a#hookSecret", "glrt-token").unwrap();
        assert_ne!(sealed, again, "a nonce per value");
        assert!(!sealed.contains("glrt"));
        let bytes = base64_decode(&sealed).unwrap();
        assert_eq!(bytes.len(), NONCE_BYTES + "glrt-token".len() + TAG_BYTES);
        assert_eq!(key.open("a#hookSecret", &sealed).unwrap(), "glrt-token");

        let other = FleetKey::from_secret("ABCDEFGHIJKLMNOPQRSTUVWXYZ012345").unwrap();
        let mut flipped = bytes.clone();
        flipped[NONCE_BYTES] ^= 1;
        for (opener, aad, value) in [
            (&other, "a#hookSecret", sealed.clone()),
            (&key, "b#hookSecret", sealed.clone()),
            (&key, "a#hookSecret", base64(&flipped)),
            (&key, "a#hookSecret", base64(&bytes[..NONCE_BYTES + 3])),
            (&key, "a#hookSecret", "not base64!".to_string()),
        ] {
            let error = opener.open(aad, &value).unwrap_err().to_string();
            assert!(error.contains(aad), "{error}");
            assert!(
                !error.contains("glrt") && !error.contains(SECRET),
                "{error}"
            );
        }
    }

    #[test]
    fn short_secrets_are_refused_without_their_value() {
        let error = FleetKey::from_secret("short-secret")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("short-secret"), "{error}");
        assert_eq!(
            format!("{:?}", FleetKey::from_secret(SECRET).unwrap()),
            "FleetKey(..)"
        );
    }

    #[test]
    fn base64_round_trips() {
        for len in 0..40usize {
            let data: Vec<u8> = (0..len as u8).map(|b| b.wrapping_mul(37)).collect();
            if data.is_empty() {
                continue;
            }
            assert_eq!(base64_decode(&base64(&data)).unwrap(), data);
        }
        for bad in ["", "abc", "a===", "ab=c", "Zg==Zg==", "####"] {
            assert_eq!(base64_decode(bad), None, "{bad}");
        }
    }
}
