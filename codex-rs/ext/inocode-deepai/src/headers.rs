//! The headers every model and account request carries.
//!
//! `api-key`, `X-DeepAI-Code-Theos` (the lane version), `X-DeepAI-Code-Instance` (random per
//! process), `X-DeepAI-Code-Machine` (hashed machine identifier; absent when none can be read) and
//! `X-Session-Affinity` (the conversation key). Redirects are never followed.

use std::sync::OnceLock;

pub const THEOS: &str = "X-DeepAI-Code-Theos";
pub const INSTANCE: &str = "X-DeepAI-Code-Instance";
pub const MACHINE: &str = "X-DeepAI-Code-Machine";
pub const AFFINITY: &str = "X-Session-Affinity";
pub const API_KEY: &str = "api-key";
pub const CHECK_IN: &str = "X-DeepAI-Code-Check-In";
pub const BEACON: &str = "X-DeepAI-Code-Beacon";

/// 16 random bytes as hex, chosen once per process.
pub fn instance_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let bytes: [u8; 16] = rand::random();
        crate::hex(&bytes)
    })
}

/// A conversation key the server accepts: 1-256 characters of letters, digits and `._:-`.
pub fn valid_affinity(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 256
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// The common headers (without the key). `affinity` is sent only when it is a valid key.
pub fn common(affinity: Option<&str>) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        (THEOS, crate::LANE_VERSION.to_string()),
        (INSTANCE, instance_id().to_string()),
    ];
    if let Some(machine) = crate::machine::machine_hash() {
        headers.push((MACHINE, machine.to_string()));
    }
    if let Some(key) = affinity.filter(|key| valid_affinity(key)) {
        headers.push((AFFINITY, key.to_string()));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instance_is_32_hex_characters_and_stable_within_the_process() {
        let id = instance_id();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(id, instance_id());
    }

    #[test]
    fn affinity_keys_are_checked() {
        assert!(valid_affinity("thread_01.a:b-c"));
        assert!(!valid_affinity(""));
        assert!(!valid_affinity("has space"));
        assert!(!valid_affinity(&"a".repeat(257)));
        assert!(valid_affinity(&"a".repeat(256)));
    }

    #[test]
    fn the_common_headers_carry_the_lane_the_instance_and_a_valid_key_only() {
        let with = common(Some("thread-1"));
        assert!(with.iter().any(|(n, v)| *n == THEOS && v == "4"));
        assert!(with.iter().any(|(n, _)| *n == INSTANCE));
        assert!(with.iter().any(|(n, v)| *n == AFFINITY && v == "thread-1"));
        assert!(!common(Some("bad key")).iter().any(|(n, _)| *n == AFFINITY));
        assert!(!common(None).iter().any(|(n, _)| *n == AFFINITY));
    }
}
