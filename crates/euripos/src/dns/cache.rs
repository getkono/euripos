//! HTTPS record lookups cached per network, each for its TTL.
//!
//! An answer is only valid on the network that gave it: a VPN's split DNS or
//! a captive network answers differently from the network before it. Entries
//! are keyed by the [`NetworkId`] the lookup ran under, so an answer from one
//! network is never served on another.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

use super::HttpsRecords;
use crate::NetworkId;

/// How many entries the cache holds before it drops the one closest to
/// expiring.
const CAPACITY: usize = 256;

/// The longest an entry is kept, whatever TTL it came with: a day, the
/// bound common resolvers apply (RFC 8767, section 4).
const MAX_TTL: Duration = Duration::from_hours(24);

/// The key of an entry: the network, where the platform names one, and the
/// name queried, lowercase.
type Key = (Option<NetworkId>, String);

/// Cached lookups.
#[derive(Debug, Default)]
pub(crate) struct Cache {
    entries: Mutex<HashMap<Key, Entry>>,
}

#[derive(Debug)]
struct Entry {
    records: HttpsRecords,
    expires: Instant,
}

impl Cache {
    /// The unexpired lookup of `name` on `network`.
    pub(crate) fn get(&self, network: Option<NetworkId>, name: &str) -> Option<HttpsRecords> {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (network, name.to_ascii_lowercase());
        match entries.get(&key) {
            Some(entry) if entry.expires > now => Some(entry.records.clone()),
            Some(_) => {
                entries.remove(&key);
                None
            }
            None => None,
        }
    }

    /// Keep `records`, the lookup of `name` on `network`, for `ttl`. A zero
    /// TTL keeps nothing (RFC 1035, section 3.2.1).
    pub(crate) fn insert(
        &self,
        network: Option<NetworkId>,
        name: &str,
        records: HttpsRecords,
        ttl: Duration,
    ) {
        if ttl.is_zero() {
            return;
        }
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (network, name.to_ascii_lowercase());
        if entries.len() >= CAPACITY && !entries.contains_key(&key) {
            entries.retain(|_, entry| entry.expires > now);
            if entries.len() >= CAPACITY {
                let soonest = entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.expires)
                    .map(|(key, _)| key.clone());
                if let Some(soonest) = soonest {
                    entries.remove(&soonest);
                }
            }
        }
        let expires = now + ttl.min(MAX_TTL);
        entries.insert(key, Entry { records, expires });
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{CAPACITY, Cache, MAX_TTL};
    use crate::NetworkId;
    use crate::dns::HttpsRecords;

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the cache is keyed by an optional network"
    )]
    fn network(key: u32) -> Option<NetworkId> {
        Some(NetworkId::from_key(&key))
    }

    #[tokio::test(start_paused = true)]
    async fn an_entry_lives_for_its_ttl() {
        let cache = Cache::default();
        cache.insert(
            network(1),
            "example.com",
            HttpsRecords::Unknown,
            Duration::from_secs(60),
        );
        tokio::time::advance(Duration::from_secs(59)).await;
        assert_eq!(
            cache.get(network(1), "example.com"),
            Some(HttpsRecords::Unknown)
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(cache.get(network(1), "example.com"), None);
        assert_eq!(cache.len(), 0, "an expired entry is dropped when read");
    }

    #[tokio::test(start_paused = true)]
    async fn entries_are_per_network() {
        let cache = Cache::default();
        cache.insert(
            network(1),
            "example.com",
            HttpsRecords::Unknown,
            Duration::from_secs(60),
        );
        assert_eq!(cache.get(network(2), "example.com"), None);
        assert_eq!(cache.get(None, "example.com"), None);
        assert_eq!(
            cache.get(network(1), "EXAMPLE.com"),
            Some(HttpsRecords::Unknown)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_ttl_is_not_cached_and_long_ttls_are_capped() {
        let cache = Cache::default();
        cache.insert(None, "zero.example", HttpsRecords::Unknown, Duration::ZERO);
        assert_eq!(cache.get(None, "zero.example"), None);
        cache.insert(
            None,
            "long.example",
            HttpsRecords::Unknown,
            Duration::from_secs(u64::from(u32::MAX)),
        );
        tokio::time::advance(MAX_TTL).await;
        assert_eq!(cache.get(None, "long.example"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_cache_drops_the_entry_closest_to_expiring() {
        let cache = Cache::default();
        for index in 0..CAPACITY {
            let ttl = Duration::from_secs(100 + u64::try_from(index).unwrap());
            cache.insert(
                None,
                &format!("{index}.example"),
                HttpsRecords::Unknown,
                ttl,
            );
        }
        cache.insert(
            None,
            "new.example",
            HttpsRecords::Unknown,
            Duration::from_secs(10),
        );
        assert_eq!(cache.len(), CAPACITY);
        assert_eq!(cache.get(None, "0.example"), None);
        assert!(cache.get(None, "1.example").is_some());
        assert!(cache.get(None, "new.example").is_some());
    }
}
