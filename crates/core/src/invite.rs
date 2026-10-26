//! Join codes (M6.3 rule R3). A code is the base64url (no padding) of the JSON
//! `{"v":1,"url":<dispatcher url>,"project":<id>,"exp":<unix>}`, a `.`, and the base64url of
//! the HMAC-SHA256 of the first part under the host's hook token. The dispatcher verifies it
//! with its hook token and stores nothing before a join.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// The version a code carries.
pub const VERSION: u64 = 1;
/// Seconds a code is valid (M6.3 rule R3).
pub const TTL_S: u64 = 7 * 24 * 3600;
/// Header of `/join`, `/leave` and `GET /fleet` that carries the code.
pub const HEADER: &str = "X-Ciburst-Join";

/// What a code grants: project `project` may join the fleet of the dispatcher at `url` until
/// `exp`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub v: u64,
    pub url: String,
    pub project: u64,
    pub exp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteError {
    /// Not two base64url parts, or no invite JSON.
    Malformed,
    /// A version this build does not know.
    Version,
    /// The signature does not match the hook token.
    Forged,
    Expired,
}

impl std::fmt::Display for InviteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InviteError::Malformed => "not a join code",
            InviteError::Version => "a join code of another version",
            InviteError::Forged => "a join code of another fleet",
            InviteError::Expired => "an expired join code: ask the host for a new one",
        })
    }
}

/// A code for `project` at the dispatcher `url`, valid for [`TTL_S`] from `now`.
pub fn issue(hook_token: &str, url: &str, project: u64, now: u64) -> String {
    let invite = Invite {
        v: VERSION,
        url: url.trim_end_matches('/').to_string(),
        project,
        exp: now + TTL_S,
    };
    let payload = encode(&serde_json::to_vec(&invite).expect("invite serializes"));
    let signature = encode(&mac(hook_token, payload.as_bytes()));
    format!("{payload}.{signature}")
}

/// The invite of `code` without checking its signature or expiry: a member reads the URL and
/// its project from it, the dispatcher alone can verify it.
pub fn decode(code: &str) -> Result<Invite, InviteError> {
    let (payload, _) = split(code)?;
    let invite: Invite = decode_bytes(payload)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(InviteError::Malformed)?;
    if invite.v != VERSION {
        return Err(InviteError::Version);
    }
    Ok(invite)
}

/// The invite of a code signed with `hook_token`, expired or not.
pub fn authentic(hook_token: &str, code: &str) -> Result<Invite, InviteError> {
    let (payload, signature) = split(code)?;
    let given = decode_bytes(signature).ok_or(InviteError::Malformed)?;
    let invite = decode(code)?;
    let expected = mac(hook_token, payload.as_bytes());
    if hook_token.is_empty() || !bool::from(expected.as_slice().ct_eq(given.as_slice())) {
        return Err(InviteError::Forged);
    }
    Ok(invite)
}

/// The invite of a code signed with `hook_token` that has not expired at `now`.
pub fn verify(hook_token: &str, code: &str, now: u64) -> Result<Invite, InviteError> {
    let invite = authentic(hook_token, code)?;
    if now >= invite.exp {
        return Err(InviteError::Expired);
    }
    Ok(invite)
}

/// The webhook secret of member `project`: the lowercase hex HMAC-SHA256 of `hook:<project>`
/// under the hook token.
pub fn hook_secret(hook_token: &str, project: u64) -> String {
    hex::encode(mac(hook_token, format!("hook:{project}").as_bytes()))
}

fn mac(key: &str, data: &[u8]) -> Vec<u8> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn split(code: &str) -> Result<(&str, &str), InviteError> {
    match code.trim().split_once('.') {
        Some((payload, signature))
            if !payload.is_empty() && !signature.is_empty() && !signature.contains('.') =>
        {
            Ok((payload, signature))
        }
        _ => Err(InviteError::Malformed),
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64url without padding.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// The bytes of base64url text without padding; `None` for any other text.
pub fn decode_bytes(text: &str) -> Option<Vec<u8>> {
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.as_bytes().chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            let value = ALPHABET.iter().position(|a| a == c)? as u32;
            n |= value << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
        // Bits below the last whole byte must be zero, so every text has one encoding.
        let used = 8 * (chunk.len() - 1);
        if n & ((1 << (24 - used)) - 1) != 0 {
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;
    const URL: &str = "https://dispatch.example";

    #[test]
    fn base64url_round_trips() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 250) as u8).collect();
            let text = encode(&bytes);
            assert!(!text.contains('='));
            assert_eq!(decode_bytes(&text), Some(bytes), "length {len}");
        }
        assert_eq!(encode(b"\xfb\xff"), "-_8");
        assert_eq!(decode_bytes("a"), None);
        assert_eq!(decode_bytes("ab+c"), None);
        assert_eq!(decode_bytes("-_9"), None, "non-zero trailing bits");
    }

    #[test]
    fn a_code_round_trips() {
        let code = issue("hook", &format!("{URL}/"), 42, NOW);
        let invite = verify("hook", &code, NOW + TTL_S - 1).unwrap();
        assert_eq!(
            invite,
            Invite {
                v: 1,
                url: URL.into(),
                project: 42,
                exp: NOW + TTL_S
            }
        );
        assert_eq!(decode(&code).unwrap(), invite);
        let (payload, _) = code.split_once('.').unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&decode_bytes(payload).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "v": 1, "url": URL, "project": 42, "exp": NOW + TTL_S })
        );
    }

    #[test]
    fn an_expired_code_is_refused() {
        let code = issue("hook", URL, 42, NOW);
        assert_eq!(
            verify("hook", &code, NOW + TTL_S),
            Err(InviteError::Expired)
        );
        assert_eq!(authentic("hook", &code).unwrap().project, 42);
    }

    #[test]
    fn a_forged_code_is_refused() {
        let code = issue("hook", URL, 42, NOW);
        assert_eq!(verify("other", &code, NOW), Err(InviteError::Forged));
        assert_eq!(verify("", &code, NOW), Err(InviteError::Forged));
        let (_, signature) = code.split_once('.').unwrap();
        let longer = Invite {
            v: 1,
            url: URL.into(),
            project: 42,
            exp: NOW + 10 * TTL_S,
        };
        let payload = encode(&serde_json::to_vec(&longer).unwrap());
        assert_eq!(
            verify("hook", &format!("{payload}.{signature}"), NOW),
            Err(InviteError::Forged)
        );
        for bad in ["", "abc", "a.b.c", ".x", "x.", "e30.AAAA"] {
            assert!(verify("hook", bad, NOW).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_code_names_one_project() {
        let code = issue("hook", URL, 42, NOW);
        let (_, signature) = code.split_once('.').unwrap();
        let other = Invite {
            v: 1,
            url: URL.into(),
            project: 43,
            exp: NOW + TTL_S,
        };
        let payload = encode(&serde_json::to_vec(&other).unwrap());
        assert_eq!(
            verify("hook", &format!("{payload}.{signature}"), NOW),
            Err(InviteError::Forged)
        );
        assert_ne!(hook_secret("hook", 42), hook_secret("hook", 43));
        assert_ne!(hook_secret("hook", 42), hook_secret("other", 42));
        let secret = hook_secret("hook", 42);
        assert_eq!(secret.len(), 64);
        assert!(
            secret
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }

    #[test]
    fn another_version_is_refused() {
        let future = encode(br#"{"v":2,"url":"https://x","project":1,"exp":1}"#);
        let signature = encode(&mac("hook", future.as_bytes()));
        assert_eq!(
            verify("hook", &format!("{future}.{signature}"), 0),
            Err(InviteError::Version)
        );
    }
}
