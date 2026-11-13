//! Password (Basic) checks that succeeded, remembered for a few minutes: a client that
//! sends its credentials on every request without keeping the session runs bcrypt once
//! and is served from here afterwards, so the cap on concurrent checks only meets
//! requests whose password still has to be checked.
//!
//! - Only successes are kept, and only in memory. A wrong password is checked and counted
//!   every time.
//! - The password is never stored: an entry is a SHA-256 over a random secret that is
//!   created at startup and never leaves the process.
//! - The hash also covers the user id and the stored password hash, so an entry ends with
//!   the password it was checked against: after any password change the old password is
//!   checked again and fails. It covers the client address as well, so an entry only
//!   serves the address whose check succeeded.
//! - An entry lives for `LIFETIME` from its check and is not renewed by hits: every
//!   client is checked at least that often.
//! - The caller looks here only for sources the login throttle has not locked.

use moka::sync::Cache;
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::time::Duration;

const LIFETIME: Duration = Duration::from_secs(5 * 60);
const MAX_ENTRIES: u64 = 10_000;

/// Names one successful check without revealing the password.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct VerifiedPassword([u8; 32]);

pub struct PasswordCache {
    secret: [u8; 32],
    verified: Cache<VerifiedPassword, ()>,
}

impl PasswordCache {
    pub fn new() -> Self {
        // two random (version 4) UUIDs carry 244 random bits
        let mut secret = [0u8; 32];
        secret[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        secret[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self {
            secret,
            // LRU rather than moka's default TinyLFU, whose admission filter can turn a
            // new key away when the cache is full
            verified: Cache::builder()
                .time_to_live(LIFETIME)
                .max_capacity(MAX_ENTRIES)
                .eviction_policy(moka::policy::EvictionPolicy::lru())
                .build(),
        }
    }

    /// The entry for this password sent from this address for the account as it is stored
    /// now. Each part is hashed with its length, so no two inputs share a key.
    pub fn key(
        &self,
        user_id: &str,
        stored_hash: &str,
        client: Option<IpAddr>,
        password: &str,
    ) -> VerifiedPassword {
        let client = client.map(|ip| ip.to_string()).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(self.secret);
        for part in [user_id, stored_hash, client.as_str(), password] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        VerifiedPassword(hasher.finalize().into())
    }

    pub fn contains(&self, key: &VerifiedPassword) -> bool {
        self.verified.get(key).is_some()
    }

    pub fn insert(&self, key: VerifiedPassword) {
        self.verified.insert(key, ());
    }
}

impl Default for PasswordCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "$2b$10$stored";

    fn address(text: &str) -> Option<IpAddr> {
        Some(text.parse().unwrap())
    }

    #[test]
    fn a_key_matches_only_the_same_user_hash_address_and_password() {
        let cache = PasswordCache::new();
        let client = address("192.0.2.1");
        let key = cache.key("user", HASH, client, "pass");
        assert!(key == cache.key("user", HASH, client, "pass"));
        assert!(key != cache.key("other", HASH, client, "pass"));
        assert!(key != cache.key("user", "$2b$10$changed", client, "pass"));
        assert!(key != cache.key("user", HASH, address("192.0.2.2"), "pass"));
        assert!(key != cache.key("user", HASH, None, "pass"));
        assert!(key != cache.key("user", HASH, client, "Pass"));
        // the parts cannot run into each other
        assert!(cache.key("ab", "c", None, "pass") != cache.key("a", "bc", None, "pass"));
    }

    #[test]
    fn keys_differ_between_processes() {
        let (one, other) = (PasswordCache::new(), PasswordCache::new());
        assert!(one.key("user", HASH, None, "pass") != other.key("user", HASH, None, "pass"));
    }

    #[test]
    fn only_inserted_keys_are_found() {
        let cache = PasswordCache::new();
        let key = cache.key("user", HASH, None, "pass");
        assert!(!cache.contains(&key));
        cache.insert(key);
        assert!(cache.contains(&key));
        assert!(!cache.contains(&cache.key("user", HASH, None, "wrong")));
    }
}
