//! Poison memory (plan 002 step 12): documents that recently timed out or
//! crashed a worker are refused before they take a worker again.
//!
//! Keyed by the upload's SHA-256, which the domain passes in `OfficeJob`,
//! `InspectJob`, `DecryptJob` and `RenderJob`. A crash that survives the
//! one retry counts two strikes, a crash with no retry one, and a timeout
//! one. At [`STRIKES`] strikes a request is refused: `convert`, `inspect`,
//! `decrypt` and `render` return `Poisoned` (422) before they lease. Crash strikes count against every request for
//! the document. A timeout strike counts only against a request that asks
//! for the same operation with no more time and no fewer pages
//! ([`Attempt::covers`]), so a retry with a longer deadline or fewer pages
//! still runs. Each strike sets the expiry to now + `POISON_TTL_SECONDS`;
//! an expired entry starts again from zero. `POISON_TTL_SECONDS=0` turns
//! it off.

use std::collections::HashMap;

use crate::domain::conversion::ExportOptions;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Strikes that poison a document.
pub const STRIKES: u32 = 2;

/// Documents remembered at most; past it the one that expires first goes.
pub const CAPACITY: usize = 1024;

/// Strikes for a call that crashed again after its retry.
pub const CRASH_STRIKES: u32 = 2;

/// Timeout strikes kept for one document; past it the oldest goes.
pub const TIMEOUTS_KEPT: usize = 8;

/// Two budgets this close count as the same. Requests with one deadline
/// differ by microseconds of scheduling, and `timeout_seconds` comes in
/// whole seconds.
pub const BUDGET_SLACK: Duration = Duration::from_millis(500);

/// What a request asks of an engine, for the timeout rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    /// An office conversion: always the whole document.
    Convert,
    /// A PDF inspect of the whole document: the page sizes, and with
    /// `probe` the blank-page probe renders too, at `dpi` for at most
    /// `pages` pages. With `password` the worker also opens the file once
    /// without it. `export` is the office export that made the PDF (`None`
    /// for a PDF upload): two exports of one upload are different PDFs.
    Inspect {
        probe: Option<Probe>,
        password: bool,
        export: Option<ExportOptions>,
    },
    /// A PDF decrypt with one password, by its keyed fingerprint
    /// ([`password_fingerprint`]): an owner password writes a copy, a user
    /// password of a restricted PDF stops at the permission check.
    Decrypt { password: u64 },
    /// A render of this many pages.
    Render { pages: u32 },
}

/// A fingerprint of a decrypt password for [`Work::Decrypt`]: SipHash under
/// a random key of this process, so the poison memory never holds the
/// password, and the value means nothing outside this process.
pub fn password_fingerprint(password: &str) -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};
    static KEY: std::sync::OnceLock<std::collections::hash_map::RandomState> =
        std::sync::OnceLock::new();
    let mut hasher = KEY
        .get_or_init(std::collections::hash_map::RandomState::new)
        .build_hasher();
    password.hash(&mut hasher);
    hasher.finish()
}

/// The blank-page probe of an inspect: its DPI and its page cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub dpi: u16,
    pub pages: u32,
}

impl Probe {
    /// Whether this probe is at least as much work as `other`: the same
    /// DPI (a higher one can skip pages that no longer fit) and no fewer
    /// pages.
    fn at_least(&self, other: &Probe) -> bool {
        self.dpi == other.dpi && self.pages >= other.pages
    }
}

/// One request for a document: its work and the time it had when the
/// engine call began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempt {
    pub work: Work,
    pub budget: Duration,
}

impl Attempt {
    pub fn new(work: Work, deadline: Instant, now: Instant) -> Self {
        Self {
            work,
            budget: deadline.saturating_duration_since(now),
        }
    }

    /// Whether this attempt's timeout counts against `next`: the same
    /// operation, no more time (within [`BUDGET_SLACK`]) and no fewer
    /// pages.
    pub fn covers(&self, next: &Attempt) -> bool {
        let work = match (self.work, next.work) {
            (Work::Convert, Work::Convert) => true,
            // Another password can take another path (Restricted, no copy).
            (
                Work::Decrypt {
                    password: timed_out,
                },
                Work::Decrypt { password },
            ) => password == timed_out,
            // A probed inspect is more work, and another export is another
            // PDF: a timeout covers only the same export, probed if it was.
            (
                Work::Inspect {
                    probe: timed_out,
                    password: timed_out_password,
                    export: timed_out_export,
                },
                Work::Inspect {
                    probe,
                    password,
                    export,
                },
            ) => {
                let probe_covered = match (timed_out, probe) {
                    (None, _) => true,
                    (Some(_), None) => false,
                    (Some(t), Some(p)) => p.at_least(&t),
                };
                probe_covered && password >= timed_out_password && export == timed_out_export
            }
            (Work::Render { pages: timed_out }, Work::Render { pages }) => pages >= timed_out,
            _ => false,
        };
        work && next.budget <= self.budget + BUDGET_SLACK
    }
}

/// What a failed engine call adds to the poison memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strike {
    /// One strike that counts only against requests it covers.
    Timeout,
    /// Strikes that count against every request for the document.
    Crash(u32),
}

impl Strike {
    fn rank(self) -> u32 {
        match self {
            Strike::Timeout => 1,
            Strike::Crash(n) => 1 + 2 * n,
        }
    }
}

/// The strike of one failed engine call, the one rule for every
/// operation: a timeout one covered strike, a crash two when it survived
/// the retry and one when there was no time for one, a protocol error or
/// the RSS kill none.
pub fn strike_for(err: &crate::engines::pool::CallError, retried: bool) -> Option<Strike> {
    use crate::engines::pool::CallError;
    match err {
        CallError::Timeout => Some(Strike::Timeout),
        CallError::Crashed { .. } if retried => Some(Strike::Crash(CRASH_STRIKES)),
        CallError::Crashed { .. } => Some(Strike::Crash(1)),
        CallError::Protocol(_) | CallError::TooBig { .. } => None,
    }
}

/// The more severe of two strikes: a crash over a timeout, two crash
/// strikes over one. One document's render chunks count once, by this.
pub fn worse(a: Option<Strike>, b: Option<Strike>) -> Option<Strike> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if y.rank() > x.rank() { y } else { x }),
        (x, None) => x,
        (None, y) => y,
    }
}

#[derive(Debug, Clone)]
struct Entry {
    crash: u32,
    timeouts: Vec<Attempt>,
    expires: Instant,
}

impl Entry {
    fn strikes_against(&self, attempt: &Attempt) -> u32 {
        let covered = self.timeouts.iter().filter(|t| t.covers(attempt)).count();
        self.crash.saturating_add(covered as u32)
    }
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

    /// Whether `attempt` on `key` meets [`STRIKES`] strikes that have not
    /// expired: every crash strike, and the timeout strikes it covers.
    pub fn refuses(&self, key: &[u8; 32], attempt: &Attempt) -> bool {
        self.refuses_at(key, attempt, Instant::now())
    }

    pub fn refuses_at(&self, key: &[u8; 32], attempt: &Attempt, now: Instant) -> bool {
        let mut entries = self.lock();
        match entries.get(key) {
            Some(entry) if entry.expires <= now => {
                entries.remove(key);
                false
            }
            Some(entry) => entry.strikes_against(attempt) >= STRIKES,
            None => false,
        }
    }

    /// Whether some request for `key` is refused now: a request like the
    /// ones that timed out, at least. For tests and status.
    pub fn is_poisoned(&self, key: &[u8; 32]) -> bool {
        let now = Instant::now();
        let mut entries = self.lock();
        match entries.get(key) {
            Some(entry) if entry.expires <= now => {
                entries.remove(key);
                false
            }
            Some(entry) => entry.crash.saturating_add(entry.timeouts.len() as u32) >= STRIKES,
            None => false,
        }
    }

    /// Adds `strike` for `attempt` to `key` and sets its expiry to now +
    /// TTL.
    pub fn strike(&self, key: &[u8; 32], strike: Strike, attempt: &Attempt) {
        self.strike_at(key, strike, attempt, Instant::now());
    }

    pub fn strike_at(&self, key: &[u8; 32], strike: Strike, attempt: &Attempt, now: Instant) {
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
            crash: 0,
            timeouts: Vec::new(),
            expires: now,
        });
        if entry.expires <= now {
            entry.crash = 0;
            entry.timeouts.clear();
        }
        let before = entry.strikes_against(attempt);
        match strike {
            Strike::Crash(n) => entry.crash = entry.crash.saturating_add(n),
            Strike::Timeout => {
                if entry.timeouts.len() >= TIMEOUTS_KEPT {
                    entry.timeouts.remove(0);
                }
                entry.timeouts.push(*attempt);
            }
        }
        entry.expires = now + self.ttl;
        let after = entry.strikes_against(attempt);
        if before < STRIKES && after >= STRIKES {
            tracing::warn!(
                sha256 = %hex12(key),
                strikes = after,
                crash_strikes = entry.crash,
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
    pub work: Work,
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    fn list(ttl_ms: u64) -> Arc<PoisonList> {
        PoisonList::new(Duration::from_millis(ttl_ms)).unwrap()
    }

    fn convert(budget_ms: u64) -> Attempt {
        Attempt {
            work: Work::Convert,
            budget: Duration::from_millis(budget_ms),
        }
    }

    fn render(pages: u32, budget_ms: u64) -> Attempt {
        Attempt {
            work: Work::Render { pages },
            budget: Duration::from_millis(budget_ms),
        }
    }

    #[test]
    fn one_rule_for_the_strikes() {
        use crate::engines::pool::CallError;
        let crash = CallError::Crashed { status: None };
        assert_eq!(
            strike_for(&CallError::Timeout, false),
            Some(Strike::Timeout)
        );
        assert_eq!(strike_for(&crash, true), Some(Strike::Crash(2)));
        assert_eq!(strike_for(&crash, false), Some(Strike::Crash(1)));
        assert_eq!(strike_for(&CallError::Protocol("x".into()), true), None);
        assert_eq!(strike_for(&CallError::TooBig { rss: 1 }, false), None);
    }

    #[test]
    fn the_worse_strike_wins() {
        let t = Some(Strike::Timeout);
        let c1 = Some(Strike::Crash(1));
        let c2 = Some(Strike::Crash(2));
        assert_eq!(worse(t, c1), c1);
        assert_eq!(worse(c2, c1), c2);
        assert_eq!(worse(None, t), t);
        assert_eq!(worse(t, None), t);
    }

    #[test]
    fn zero_ttl_turns_it_off() {
        assert!(PoisonList::new(Duration::ZERO).is_none());
    }

    #[test]
    fn two_timeouts_or_one_double_crash_poison() {
        let now = Instant::now();
        let l = list(600_000);
        let a = convert(5_000);
        l.strike_at(&KEY, Strike::Timeout, &a, now);
        assert!(!l.refuses_at(&KEY, &a, now));
        l.strike_at(&KEY, Strike::Timeout, &a, now);
        assert!(l.refuses_at(&KEY, &a, now));
        let other = [8; 32];
        l.strike_at(&other, Strike::Crash(CRASH_STRIKES), &a, now);
        assert!(l.refuses_at(&other, &render(1, 1_000_000), now));
        assert!(!l.refuses_at(&[9; 32], &a, now));
    }

    #[test]
    fn a_timeout_counts_only_against_requests_it_covers() {
        let now = Instant::now();
        let l = list(600_000);
        let short = convert(5_000);
        l.strike_at(&KEY, Strike::Timeout, &short, now);
        l.strike_at(&KEY, Strike::Timeout, &short, now);
        assert!(l.refuses_at(&KEY, &short, now));
        assert!(l.refuses_at(&KEY, &convert(1_000), now), "less time");
        assert!(l.refuses_at(&KEY, &convert(5_400), now), "within the slack");
        assert!(!l.refuses_at(&KEY, &convert(6_000), now), "more time");
        assert!(
            !l.refuses_at(&KEY, &render(20, 5_000), now),
            "another operation"
        );

        let pages = [3; 32];
        let twenty = render(20, 5_000);
        l.strike_at(&pages, Strike::Timeout, &twenty, now);
        l.strike_at(&pages, Strike::Timeout, &twenty, now);
        assert!(l.refuses_at(&pages, &twenty, now));
        assert!(l.refuses_at(&pages, &render(40, 5_000), now), "more pages");
        assert!(!l.refuses_at(&pages, &render(5, 5_000), now), "fewer pages");
        assert!(!l.refuses_at(&pages, &render(20, 30_000), now), "more time");
    }

    #[test]
    fn a_probed_inspect_timeout_does_not_cover_an_unprobed_inspect() {
        let now = Instant::now();
        let l = list(600_000);
        let probe_200 = Probe {
            dpi: 18,
            pages: 200,
        };
        let inspect = |probe: bool, export| Attempt {
            work: Work::Inspect {
                probe: probe.then_some(probe_200),
                password: false,
                export,
            },
            budget: Duration::from_secs(10),
        };
        let probed = inspect(true, None);
        let plain = inspect(false, None);
        l.strike_at(&KEY, Strike::Timeout, &probed, now);
        l.strike_at(&KEY, Strike::Timeout, &probed, now);
        assert!(l.refuses_at(&KEY, &probed, now));
        assert!(!l.refuses_at(&KEY, &plain, now), "less work");
        let other = [21; 32];
        l.strike_at(&other, Strike::Timeout, &plain, now);
        l.strike_at(&other, Strike::Timeout, &plain, now);
        assert!(l.refuses_at(&other, &plain, now));
        assert!(l.refuses_at(&other, &probed, now), "more work");
        // Another export of the same upload is another PDF.
        let notes = ExportOptions {
            notes: crate::domain::conversion::SlideNotes::Append,
            ..ExportOptions::default()
        };
        let slides = Some(ExportOptions::default());
        let third = [22; 32];
        l.strike_at(&third, Strike::Timeout, &inspect(false, slides), now);
        l.strike_at(&third, Strike::Timeout, &inspect(false, slides), now);
        assert!(l.refuses_at(&third, &inspect(false, slides), now));
        assert!(
            !l.refuses_at(&third, &inspect(false, Some(notes)), now),
            "another export"
        );
        assert!(
            !l.refuses_at(&third, &inspect(false, None), now),
            "not an export"
        );
        // A password inspect opens the file twice: its timeout does not
        // cover one without a password.
        let with_password = Attempt {
            work: Work::Inspect {
                probe: None,
                password: true,
                export: None,
            },
            budget: Duration::from_secs(10),
        };
        let fourth = [23; 32];
        l.strike_at(&fourth, Strike::Timeout, &with_password, now);
        l.strike_at(&fourth, Strike::Timeout, &with_password, now);
        assert!(l.refuses_at(&fourth, &with_password, now));
        assert!(
            !l.refuses_at(&fourth, &inspect(false, None), now),
            "one open"
        );
        // A probe of fewer pages, or at a lower DPI, is less work.
        let smaller = |dpi, pages| Attempt {
            work: Work::Inspect {
                probe: Some(Probe { dpi, pages }),
                password: false,
                export: None,
            },
            budget: Duration::from_secs(10),
        };
        assert!(
            !l.refuses_at(&KEY, &smaller(18, 1), now),
            "fewer probe pages"
        );
        assert!(
            !l.refuses_at(&KEY, &smaller(9, 200), now),
            "another probe DPI"
        );
        assert!(
            !l.refuses_at(&KEY, &smaller(36, 400), now),
            "another probe DPI"
        );
        assert!(
            l.refuses_at(&KEY, &smaller(18, 400), now),
            "more probe pages"
        );
    }

    #[test]
    fn a_decrypt_timeout_covers_only_the_same_password() {
        let now = Instant::now();
        let l = list(600_000);
        let decrypt = |password: &str| Attempt {
            work: Work::Decrypt {
                password: password_fingerprint(password),
            },
            budget: Duration::from_secs(10),
        };
        l.strike_at(&KEY, Strike::Timeout, &decrypt("owner"), now);
        l.strike_at(&KEY, Strike::Timeout, &decrypt("owner"), now);
        assert!(l.refuses_at(&KEY, &decrypt("owner"), now));
        assert!(
            !l.refuses_at(&KEY, &decrypt("user"), now),
            "another password"
        );
        assert_eq!(password_fingerprint("owner"), password_fingerprint("owner"));
    }

    #[test]
    fn a_crash_counts_against_every_request() {
        let now = Instant::now();
        let l = list(600_000);
        l.strike_at(&KEY, Strike::Crash(1), &convert(5_000), now);
        l.strike_at(&KEY, Strike::Timeout, &convert(5_000), now);
        assert!(l.refuses_at(&KEY, &convert(5_000), now));
        assert!(
            !l.refuses_at(&KEY, &convert(60_000), now),
            "one crash strike"
        );
        l.strike_at(&KEY, Strike::Crash(1), &convert(60_000), now);
        assert!(l.refuses_at(&KEY, &convert(60_000), now));
        assert!(l.refuses_at(&KEY, &render(1, 60_000), now));
    }

    #[test]
    fn strikes_expire_after_the_ttl() {
        let now = Instant::now();
        let l = list(1_000);
        let a = convert(5_000);
        l.strike_at(&KEY, Strike::Crash(CRASH_STRIKES), &a, now);
        assert!(l.refuses_at(&KEY, &a, now + Duration::from_millis(999)));
        assert!(!l.refuses_at(&KEY, &a, now + Duration::from_millis(1_000)));
        // An expired entry starts from zero again.
        let later = now + Duration::from_secs(5);
        l.strike_at(&KEY, Strike::Timeout, &a, later);
        assert!(!l.refuses_at(&KEY, &a, later));
    }

    #[test]
    fn the_list_stays_within_its_capacity() {
        let now = Instant::now();
        let l = list(600_000);
        let a = convert(5_000);
        for i in 0..CAPACITY + 10 {
            let mut key = [0u8; 32];
            key[..8].copy_from_slice(&(i as u64).to_le_bytes());
            l.strike_at(
                &key,
                Strike::Timeout,
                &a,
                now + Duration::from_millis(i as u64),
            );
        }
        assert_eq!(l.len(), CAPACITY);
        // The oldest entries went first.
        let mut first = [0u8; 32];
        first[..8].copy_from_slice(&0u64.to_le_bytes());
        l.strike_at(&first, Strike::Timeout, &a, now + Duration::from_secs(1));
        assert!(!l.refuses_at(&first, &a, now + Duration::from_secs(1)));
    }

    #[test]
    fn one_document_keeps_a_bounded_number_of_timeouts() {
        let now = Instant::now();
        let l = list(600_000);
        for budget in 0..TIMEOUTS_KEPT as u64 + 4 {
            l.strike_at(&KEY, Strike::Timeout, &convert(budget * 10_000), now);
        }
        assert_eq!(l.lock()[&KEY].timeouts.len(), TIMEOUTS_KEPT);
    }
}
