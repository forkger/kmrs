//! Failed password logins per account and client address. After `MAX_FAILURES` failures in
//! a row from one address (none more than `FAILURE_WINDOW` apart, counted from the end of a
//! lock) that address is refused password logins for the account for a while: one minute at
//! first, doubling with every further failure up to `MAX_LOCK`. A successful login from the
//! address clears its count.
//!
//! - Keyed by user id and client address, so a lock only hits the source that is guessing:
//!   the user's other devices keep signing in, and knowing an email is not enough to lock
//!   its owner out. The address is the one `http::client_addr` resolves, which takes
//!   `X-Forwarded-For` from trusted proxies only. Clients that reach the server through one
//!   shared address, such as a proxy that is not listed, share a key.
//! - IPv6 addresses are counted by their /64 network: a client usually holds a whole /64
//!   and can pick any address in it, so counting each address on its own would give it
//!   fresh attempts and a new entry per address. Wherever this file says address, that is
//!   the /64 network for IPv6: its addresses share a count and a lock, and a success from
//!   one of them clears both. IPv4 addresses are counted as they are.
//! - Only existing accounts are counted, so guessing made-up emails cannot grow the map.
//! - Only password (Basic) logins are refused. Sessions, API keys and remember-me cookies
//!   keep working, so devices that are already signed in are not affected.
//! - A failure counts when its check fails, never before, so a client that sends many
//!   requests at once with the right password is never refused. The caller asks
//!   `is_locked` again right before each check runs: of a burst of wrong guesses, only
//!   the checks that were already running when the lock started still finish.
//! - A refused attempt answers like a wrong password without checking it, and does not
//!   extend the lock: retrying while locked never makes the wait longer. The lock only
//!   grows when a check fails after it has ended.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Keeps the upper 64 bits of an IPv6 address, its /64 network.
const IPV6_NETWORK_MASK: u128 = !(u64::MAX as u128);
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(10 * 60);
const FIRST_LOCK: Duration = Duration::from_secs(60);
const MAX_LOCK: Duration = Duration::from_secs(15 * 60);

struct Failures {
    count: u32,
    last: Instant,
    lock: Duration,
}

impl Failures {
    /// Kept while a further failure would still count: until `FAILURE_WINDOW` after the
    /// last failure or, when locked, after the lock ends (a lock can outlast the window).
    fn is_live(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last + self.lock) < FAILURE_WINDOW
    }

    fn is_locked(&self, now: Instant) -> bool {
        !self.lock.is_zero() && now < self.last + self.lock
    }
}

/// An account as seen from one client address, or from one /64 network for IPv6. The
/// address is `None` only when the connection's own address is unknown.
type Source = (String, Option<IpAddr>);

/// The key an attempt is counted under: an IPv6 address is reduced to its /64 network.
fn source(user_id: &str, client: Option<IpAddr>) -> Source {
    let counted = client.map(|address| match address {
        IpAddr::V4(_) => address,
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & IPV6_NETWORK_MASK)),
    });
    (user_id.to_string(), counted)
}

#[derive(Default)]
pub struct LoginThrottle {
    sources: Mutex<HashMap<Source, Failures>>,
}

impl LoginThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts a failed password check for the account from the client address and returns
    /// the lock it starts, if it starts one. A failure that arrives while the source is
    /// locked, from a check that was already running, is not counted.
    pub fn record_failure(
        &self,
        user_id: &str,
        client: Option<IpAddr>,
        now: Instant,
    ) -> Option<Duration> {
        let key = source(user_id, client);
        let mut sources = self.sources.lock().unwrap();
        if sources
            .get(&key)
            .is_some_and(|failures| failures.is_locked(now))
        {
            return None;
        }
        if !sources.contains_key(&key) {
            // a new key is the only way the map grows: drop the entries that are done
            sources.retain(|_, failures| failures.is_live(now));
        }
        let failures = sources.entry(key).or_insert_with(|| Failures {
            count: 0,
            last: now,
            lock: Duration::ZERO,
        });
        if !failures.is_live(now) {
            *failures = Failures {
                count: 0,
                last: now,
                lock: Duration::ZERO,
            };
        }
        failures.count += 1;
        failures.last = now;
        if failures.count >= MAX_FAILURES {
            failures.lock = if failures.lock.is_zero() {
                FIRST_LOCK
            } else {
                (failures.lock * 2).min(MAX_LOCK)
            };
            return Some(failures.lock);
        }
        None
    }

    /// A successful password login clears the failures of that account and address (for
    /// IPv6, of its /64 network).
    pub fn record_success(&self, user_id: &str, client: Option<IpAddr>) {
        self.sources
            .lock()
            .unwrap()
            .remove(&source(user_id, client));
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sources.lock().unwrap().len()
    }

    /// Whether that source is refused right now, without counting anything.
    pub fn is_locked(&self, user_id: &str, client: Option<IpAddr>, now: Instant) -> bool {
        self.sources
            .lock()
            .unwrap()
            .get(&source(user_id, client))
            .is_some_and(|failures| failures.is_locked(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two client addresses.
    const A: Option<IpAddr> = Some(IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 1)));
    const B: Option<IpAddr> = Some(IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 2)));

    fn fail_times(throttle: &LoginThrottle, user_id: &str, start: Instant, n: u32) -> Instant {
        let mut now = start;
        for _ in 0..n {
            throttle.record_failure(user_id, A, now);
            now += Duration::from_secs(1);
        }
        now
    }

    #[test]
    fn locks_after_max_failures_for_one_minute() {
        let throttle = LoginThrottle::new();
        let t0 = Instant::now();
        let after_four = fail_times(&throttle, "u1", t0, MAX_FAILURES - 1);
        assert!(!throttle.is_locked("u1", A, after_four));

        assert_eq!(
            throttle.record_failure("u1", A, after_four),
            Some(FIRST_LOCK)
        );
        assert!(throttle.is_locked("u1", A, after_four));
        assert!(throttle.is_locked("u1", A, after_four + Duration::from_secs(59)));
        assert!(!throttle.is_locked("u1", A, after_four + FIRST_LOCK));
        // other accounts are unaffected
        assert!(!throttle.is_locked("u2", A, after_four));
    }

    #[test]
    fn a_failure_while_locked_is_not_counted() {
        let throttle = LoginThrottle::new();
        let now = fail_times(&throttle, "u1", Instant::now(), MAX_FAILURES);
        // checks that were already running when the lock started
        for _ in 0..3 {
            assert_eq!(throttle.record_failure("u1", A, now), None);
        }
        // the lock is neither longer nor later for them
        let lock_start = now - Duration::from_secs(1);
        assert!(throttle.is_locked("u1", A, lock_start + FIRST_LOCK - Duration::from_secs(1)));
        assert!(!throttle.is_locked("u1", A, lock_start + FIRST_LOCK));
        // and the next failure after it doubles it once, not once per uncounted failure
        assert_eq!(
            throttle.record_failure("u1", A, lock_start + FIRST_LOCK),
            Some(FIRST_LOCK * 2)
        );
    }

    #[test]
    fn a_lock_holds_for_one_address_only() {
        let throttle = LoginThrottle::new();
        let now = fail_times(&throttle, "u1", Instant::now(), MAX_FAILURES);
        assert!(throttle.is_locked("u1", A, now));
        // the same account from another address is neither locked nor counted so far
        assert!(!throttle.is_locked("u1", B, now));
        assert_eq!(throttle.record_failure("u1", B, now), None);
        assert!(!throttle.is_locked("u1", B, now));
        // a success from the other address leaves the first one locked
        throttle.record_success("u1", B);
        assert!(throttle.is_locked("u1", A, now));
        // an unknown address is a source of its own
        assert_eq!(throttle.record_failure("u1", None, now), None);
        assert!(!throttle.is_locked("u1", None, now));
        assert!(throttle.is_locked("u1", A, now));
    }

    #[test]
    fn ipv6_addresses_in_one_64_network_share_a_lock() {
        fn v6(text: &str) -> Option<IpAddr> {
            Some(text.parse().unwrap())
        }
        let first = v6("2001:db8:0:1::1");
        let same_network = v6("2001:db8:0:1:ffff:ffff:ffff:fffe");
        // differs from the first network in its last bit only
        let other_network = v6("2001:db8::1");
        // differs from it in its first 16 bits only
        let far_network = v6("2002:db8:0:1::1");

        let throttle = LoginThrottle::new();
        let mut now = Instant::now();
        // the failures come from two addresses of one network and count together
        for i in 0..MAX_FAILURES {
            let client = if i % 2 == 0 { first } else { same_network };
            throttle.record_failure("u1", client, now);
            now += Duration::from_secs(1);
        }
        assert!(throttle.is_locked("u1", first, now));
        assert!(throttle.is_locked("u1", same_network, now));
        assert!(throttle.is_locked("u1", v6("2001:db8:0:1::2"), now));
        assert_eq!(throttle.len(), 1);
        // the neighbouring network is neither locked nor counted so far
        assert!(!throttle.is_locked("u1", other_network, now));
        assert_eq!(throttle.record_failure("u1", other_network, now), None);
        assert!(!throttle.is_locked("u1", other_network, now));
        assert_eq!(throttle.len(), 2);
        // nor is a network that only shares the lower part of the prefix
        assert!(!throttle.is_locked("u1", far_network, now));
        assert_eq!(throttle.record_failure("u1", far_network, now), None);
        assert_eq!(throttle.len(), 3);
        // a success from any address of the network clears it, and only it
        throttle.record_success("u1", v6("2001:db8:0:1::3"));
        assert!(!throttle.is_locked("u1", first, now));
        assert_eq!(throttle.len(), 2);
    }

    #[test]
    fn each_further_failure_doubles_the_lock_up_to_the_cap() {
        let throttle = LoginThrottle::new();
        let mut now = fail_times(&throttle, "u1", Instant::now(), MAX_FAILURES);
        let mut expected = FIRST_LOCK;
        for _ in 0..6 {
            now += expected; // the lock has just run out
            expected = (expected * 2).min(MAX_LOCK);
            assert_eq!(throttle.record_failure("u1", A, now), Some(expected));
            assert!(throttle.is_locked("u1", A, now + expected - Duration::from_secs(1)));
            assert!(!throttle.is_locked("u1", A, now + expected));
        }
        assert_eq!(expected, MAX_LOCK);
    }

    #[test]
    fn success_and_quiet_periods_reset_the_count() {
        let throttle = LoginThrottle::new();
        let t0 = Instant::now();
        let now = fail_times(&throttle, "u1", t0, MAX_FAILURES - 1);
        throttle.record_success("u1", A);
        let now = fail_times(&throttle, "u1", now, MAX_FAILURES - 1);
        assert!(!throttle.is_locked("u1", A, now));

        // failures further apart than the window start over
        let later = now + FAILURE_WINDOW;
        assert_eq!(throttle.record_failure("u1", A, later), None);
        assert!(!throttle.is_locked("u1", A, later));
    }

    #[test]
    fn finished_entries_are_dropped_when_a_new_source_fails() {
        let throttle = LoginThrottle::new();
        let t0 = Instant::now();
        throttle.record_failure("u1", A, t0);
        fail_times(&throttle, "u2", t0, MAX_FAILURES);
        assert_eq!(throttle.len(), 2);

        // u1's single failure is past the window; u2's lock ended less than a window ago
        let later = t0 + FAILURE_WINDOW;
        throttle.record_failure("u3", A, later);
        assert_eq!(throttle.len(), 2);
        assert!(!throttle
            .sources
            .lock()
            .unwrap()
            .contains_key(&("u1".to_string(), A)));
    }

    #[test]
    fn a_success_clears_the_count_and_a_lock() {
        let throttle = LoginThrottle::new();
        let now = fail_times(&throttle, "u1", Instant::now(), MAX_FAILURES);
        assert!(throttle.is_locked("u1", A, now));
        // a check that was already running when the lock started, with the right password
        throttle.record_success("u1", A);
        assert!(!throttle.is_locked("u1", A, now));
        assert_eq!(throttle.len(), 0);
    }
}
