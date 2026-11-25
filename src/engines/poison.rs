//! Poison memory (plan 002 step 12): documents that recently timed out or
//! crashed a worker are refused before they take a worker again.
//!
//! Keyed by the upload's SHA-256, which the domain passes in `OfficeJob`
//! and `RenderJob`. A timeout counts one strike; a crash that survives the
//! one retry counts two. At [`STRIKES`] strikes the document is poisoned:
//! `convert` and `render` return `Poisoned` (422) before they lease. Each
//! strike sets the expiry to now + `POISON_TTL_SECONDS`; an expired entry
//! starts again from zero. `POISON_TTL_SECONDS=0` turns it off.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Strikes that poison a document.
pub const STRIKES: u32 = 2;

/// Documents remembered at most; past it the one that expires first goes.
pub const CAPACITY: usize = 1024;

/// Strikes for a call that timed out.
pub const TIMEOUT_STRIKES: u32 = 1;

/// Strikes for a call that crashed again after its retry.
pub const CRASH_STRIKES: u32 = 2;

#[derive(Debug, Clone, Copy)]
struct Entry {
    strikes: u32,
    expires: Instant,
}

/// The poison memory of one `serve` process, shared by both pools.
#[derive(Debug)]
pub struct PoisonList {
    ttl: Duration,
    entries: Mutex<HashMap<[u8; 32], Entry>>,
}

fn hex12(key: &[u8; 32]) -> String {
    key.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

impl PoisonList {
    /// A poison list with entries that live `ttl`; `None` when `ttl` is 0.
    pub fn new(ttl: Duration) -> Option<Arc<Self>> {
        (!ttl.is_zero()).then(|| {
            Arc::new(Self {
                ttl,
                entries: Mutex::new(HashMap::new()),
            })
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<[u8; 32], Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether `key` has [`STRIKES`] strikes that have not expired.
    pub fn is_poisoned(&self, key: &[u8; 32]) -> bool {
        self.is_poisoned_at(key, Instant::now())
    }

    pub fn is_poisoned_at(&self, key: &[u8; 32], now: Instant) -> bool {
        let mut entries = self.lock();
        match entries.get(key) {
            Some(entry) if entry.expires <= now => {
                entries.remove(key);
                false
            }
            Some(entry) => entry.strikes >= STRIKES,
            None => false,
        }
    }

    /// Adds `strikes` to `key` and sets its expiry to now + TTL.
    pub fn strike(&self, key: &[u8; 32], strikes: u32) {
        self.strike_at(key, strikes, Instant::now());
    }

    pub fn strike_at(&self, key: &[u8; 32], strikes: u32, now: Instant) {
        let mut entries = self.lock();
        if !entries.contains_key(key) && entries.len() >= CAPACITY {
            entries.retain(|_, e| e.expires > now);
            if entries.len() >= CAPACITY
                && let Some(oldest) = entries
                    .iter()
                    .min_by_key(|(_, e)| e.expires)
                    .map(|(k, _)| *k)
            {
                entries.remove(&oldest);
            }
        }
        let entry = entries.entry(*key).or_insert(Entry {
            strikes: 0,
            expires: now,
        });
        if entry.expires <= now {
            entry.strikes = 0;
        }
        let before = entry.strikes;
        entry.strikes = entry.strikes.saturating_add(strikes);
        entry.expires = now + self.ttl;
        if before < STRIKES && entry.strikes >= STRIKES {
            tracing::warn!(
                sha256 = %hex12(key),
                strikes = entry.strikes,
                ttl_s = self.ttl.as_secs(),
                "document poisoned"
            );
        }
    }

    /// Entries held now, expired ones included (for tests).
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One document's place in the poison list, for an engine call.
#[derive(Debug, Clone)]
pub struct Poison {
    pub list: Arc<PoisonList>,
    pub key: [u8; 32],
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    fn list(ttl_ms: u64) -> Arc<PoisonList> {
        PoisonList::new(Duration::from_millis(ttl_ms)).unwrap()
    }

    #[test]
    fn zero_ttl_turns_it_off() {
        assert!(PoisonList::new(Duration::ZERO).is_none());
    }

    #[test]
    fn two_timeouts_or_one_double_crash_poison() {
        let now = Instant::now();
        let l = list(600_000);
        l.strike_at(&KEY, TIMEOUT_STRIKES, now);
        assert!(!l.is_poisoned_at(&KEY, now));
        l.strike_at(&KEY, TIMEOUT_STRIKES, now);
        assert!(l.is_poisoned_at(&KEY, now));
        let other = [8; 32];
        l.strike_at(&other, CRASH_STRIKES, now);
        assert!(l.is_poisoned_at(&other, now));
        assert!(!l.is_poisoned_at(&[9; 32], now));
    }

    #[test]
    fn strikes_expire_after_the_ttl() {
        let now = Instant::now();
        let l = list(1_000);
        l.strike_at(&KEY, CRASH_STRIKES, now);
        assert!(l.is_poisoned_at(&KEY, now + Duration::from_millis(999)));
        assert!(!l.is_poisoned_at(&KEY, now + Duration::from_millis(1_000)));
        // An expired entry starts from zero again.
        let later = now + Duration::from_secs(5);
        l.strike_at(&KEY, TIMEOUT_STRIKES, later);
        assert!(!l.is_poisoned_at(&KEY, later));
    }

    #[test]
    fn the_list_stays_within_its_capacity() {
        let now = Instant::now();
        let l = list(600_000);
        for i in 0..CAPACITY + 10 {
            let mut key = [0u8; 32];
            key[..8].copy_from_slice(&(i as u64).to_le_bytes());
            l.strike_at(&key, TIMEOUT_STRIKES, now + Duration::from_millis(i as u64));
        }
        assert_eq!(l.len(), CAPACITY);
        // The oldest entries went first.
        let mut first = [0u8; 32];
        first[..8].copy_from_slice(&0u64.to_le_bytes());
        l.strike_at(&first, TIMEOUT_STRIKES, now + Duration::from_secs(1));
        assert!(!l.is_poisoned_at(&first, now + Duration::from_secs(1)));
    }
}
