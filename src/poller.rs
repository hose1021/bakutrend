//! The poll cycle: fetch every due source, store what comes back, sample view counts,
//! prune old samples. Sources are isolated — one failure never aborts the cycle.

use std::collections::HashMap;

use crate::error::StoreError;
use crate::source::SourceKind;
use crate::source::http::Fetcher;
use crate::store::Store;

const BACKOFF_BASE_SECS: i64 = 60;
const BACKOFF_CAP_SECS: i64 = 1800;

#[derive(Debug, Default)]
pub struct Backoff {
    failures: HashMap<i64, u32>,
    ready_at: HashMap<i64, i64>,
}

impl Backoff {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_due(&self, source_id: i64, now: i64) -> bool {
        self.ready_at
            .get(&source_id)
            .is_none_or(|ready| now >= *ready)
    }

    pub fn record_ok(&mut self, source_id: i64) {
        self.failures.remove(&source_id);
        self.ready_at.remove(&source_id);
    }

    pub fn record_failure(&mut self, source_id: i64, now: i64) {
        let attempts = self.failures.entry(source_id).or_insert(0);
        *attempts += 1;
        let delay = (BACKOFF_BASE_SECS * 2i64.pow((*attempts - 1).min(10))).min(BACKOFF_CAP_SECS);
        self.ready_at.insert(source_id, now + delay);
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollReport {
    pub ok: usize,
    pub failed: Vec<(String, String)>,
    pub new_items: usize,
    pub samples: usize,
    pub pruned: usize,
    pub skipped: usize,
}

/// One full cycle. Sources that are backing off are skipped, so `ok + failed` can be
/// smaller than the enabled source count.
pub fn poll_once(
    store: &mut Store,
    fetcher: &dyn Fetcher,
    backoff: &mut Backoff,
    now: i64,
    retention_days: i64,
) -> Result<PollReport, StoreError> {
    let mut report = PollReport::default();
    let sources = store.sources(true)?;
    let has_week_data = !store.window(crate::store::Window::Week, now)?.0.is_empty();

    // The Google source is a one-shot seed, never a per-cycle poll.
    for source in sources.iter().filter(|s| s.kind != SourceKind::Google) {
        if !backoff.is_due(source.id, now) {
            continue;
        }
        match fetcher.fetch(source) {
            Ok(outcome) => {
                backoff.record_ok(source.id);
                report.new_items += store.upsert_items(source.id, &outcome.items, now)?;
                report.skipped += outcome.skipped;
                report.ok += 1;
            }
            Err(error) => {
                backoff.record_failure(source.id, now);
                report.failed.push((source.name.clone(), error.to_string()));
                log::warn!("source {} failed: {error}", source.name);
            }
        }
    }

    // One-shot week seed: only when no locally observed week data exists yet, and on the same
    // backoff as every other source. A failed seed leaves the week empty, so without the guard
    // an unreachable Google would be retried every cycle forever.
    if !has_week_data
        && let Some(seed) = sources.iter().find(|s| s.kind == SourceKind::Google)
        && backoff.is_due(seed.id, now)
    {
        match fetcher.fetch(seed) {
            Ok(outcome) => {
                backoff.record_ok(seed.id);
                store.upsert_items(seed.id, &outcome.items, now)?;
            }
            Err(error) => {
                backoff.record_failure(seed.id, now);
                log::warn!("google backfill failed: {error}");
                report.failed.push((seed.name.clone(), error.to_string()));
            }
        }
    }

    report.samples = store.sample_views(now)?;
    report.pruned = store.prune_samples(now - retention_days * 86_400)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::FetchError;
    use crate::source::http::Fetcher;
    use crate::source::{ParseOutcome, SourceKind, SourceSpec};

    struct NeverFetcher;

    impl Fetcher for NeverFetcher {
        fn fetch(&self, _source: &crate::store::SourceRow) -> Result<ParseOutcome, FetchError> {
            Err(FetchError::Http {
                status: 500,
                url: "https://example.az".to_string(),
            })
        }
    }

    struct CountingFetcher {
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl Fetcher for CountingFetcher {
        fn fetch(&self, source: &crate::store::SourceRow) -> Result<ParseOutcome, FetchError> {
            self.calls.borrow_mut().push(source.locator.clone());
            Ok(ParseOutcome::default())
        }
    }

    fn store_with_two_sources() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Rss,
                    outlet: "APA".into(),
                    name: "APA RSS".into(),
                    locator: "https://apa.az/rss".into(),
                },
                true,
            )
            .unwrap();
        store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Telegram,
                    outlet: "APA".into(),
                    name: "APA Telegram".into(),
                    locator: "@apa_az".into(),
                },
                true,
            )
            .unwrap();
        store
    }

    #[test]
    fn a_failing_source_is_reported_and_does_not_stop_the_others() {
        let mut store = store_with_two_sources();
        let mut backoff = Backoff::new();
        let report = poll_once(&mut store, &NeverFetcher, &mut backoff, 1_700_000_000, 30).unwrap();

        assert_eq!(report.ok, 0);
        assert_eq!(report.failed.len(), 2);
        assert_eq!(report.failed[0].0, "APA RSS");
        assert!(report.failed[0].1.contains("500"));
    }

    #[test]
    fn a_failed_source_backs_off_then_returns_within_the_cap() {
        let mut backoff = Backoff::new();
        let now = 1_700_000_000;
        assert!(backoff.is_due(1, now));
        backoff.record_failure(1, now);
        assert!(
            !backoff.is_due(1, now + 30),
            "not due immediately after a failure"
        );
        assert!(
            backoff.is_due(1, now + 61),
            "due after the first backoff interval"
        );

        for step in 1..=12 {
            backoff.record_failure(1, now + step * 3600);
        }
        assert!(
            !backoff.is_due(1, now + 12 * 3600 + 1700),
            "never exceeds the 30 minute cap"
        );
        assert!(backoff.is_due(1, now + 12 * 3600 + 1801));

        backoff.record_ok(1);
        assert!(
            backoff.is_due(1, now + 12 * 3600 + 1802),
            "a success clears the backoff"
        );
    }

    #[test]
    fn polling_twice_changes_nothing_the_second_time() {
        let mut store = store_with_two_sources();
        let mut backoff = Backoff::new();
        let fetcher = CountingFetcher {
            calls: std::cell::RefCell::new(Vec::new()),
        };
        let now = 1_700_000_000;

        let first = poll_once(&mut store, &fetcher, &mut backoff, now, 30).unwrap();
        let second = poll_once(&mut store, &fetcher, &mut backoff, now + 60, 30).unwrap();
        assert_eq!(first.ok, 2);
        assert_eq!(second.ok, 2);
        assert_eq!(store.item_count().unwrap(), 0);
    }

    fn google_source(store: &mut Store) -> i64 {
        store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Google,
                    outlet: "Google News".into(),
                    name: "Google News AZ".into(),
                    locator: "https://news.google.com/rss/search?q=az".into(),
                },
                true,
            )
            .unwrap()
    }

    /// Records every attempt and always fails, so a cycle's retries are observable.
    struct AlwaysFailingFetcher {
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl Fetcher for AlwaysFailingFetcher {
        fn fetch(&self, source: &crate::store::SourceRow) -> Result<ParseOutcome, FetchError> {
            self.calls.borrow_mut().push(source.locator.clone());
            Err(FetchError::Http {
                status: 500,
                url: source.locator.clone(),
            })
        }
    }

    #[test]
    fn a_failing_google_seed_backs_off_instead_of_retrying_every_cycle() {
        let mut store = store_with_two_sources();
        google_source(&mut store);
        let fetcher = AlwaysFailingFetcher {
            calls: std::cell::RefCell::new(Vec::new()),
        };
        let mut backoff = Backoff::new();
        let now = 1_700_000_000;
        let seeds = || {
            fetcher
                .calls
                .borrow()
                .iter()
                .filter(|locator| locator.contains("news.google.com"))
                .count()
        };

        let first = poll_once(&mut store, &fetcher, &mut backoff, now, 30).unwrap();
        assert_eq!(seeds(), 1, "an empty week triggers the seed");
        assert!(
            first
                .failed
                .iter()
                .any(|(name, _)| name == "Google News AZ")
        );

        let second = poll_once(&mut store, &fetcher, &mut backoff, now + 30, 30).unwrap();
        assert_eq!(
            seeds(),
            1,
            "a failed seed waits out its backoff, it does not retry every cycle"
        );
        assert_eq!(
            fetcher.calls.borrow().len(),
            3,
            "nothing is retried while backing off"
        );
        assert!(
            second.failed.is_empty(),
            "a skipped source is not reported as a failure"
        );
    }

    /// Serves one fresh item for every feed source, so a cycle leaves week data behind.
    struct OneItemFetcher {
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl Fetcher for OneItemFetcher {
        fn fetch(&self, source: &crate::store::SourceRow) -> Result<ParseOutcome, FetchError> {
            self.calls.borrow_mut().push(source.locator.clone());
            if source.kind == SourceKind::Google {
                return Ok(ParseOutcome::default());
            }
            Ok(ParseOutcome {
                items: vec![crate::source::ParsedItem {
                    external_id: format!("{}-1", source.id),
                    url: format!("https://example.az/{}", source.id),
                    title: "Yeni xəbər".into(),
                    description: None,
                    section: None,
                    published_at: 1_700_000_000,
                    views: None,
                    cited: false,
                    publisher: None,
                }],
                skipped: 0,
            })
        }
    }

    #[test]
    fn the_google_source_is_seeded_once_and_never_polled_again() {
        let mut store = store_with_two_sources();
        google_source(&mut store);
        let fetcher = OneItemFetcher {
            calls: std::cell::RefCell::new(Vec::new()),
        };
        let mut backoff = Backoff::new();
        let now = 1_700_000_000;

        let first = poll_once(&mut store, &fetcher, &mut backoff, now, 30).unwrap();
        let second = poll_once(&mut store, &fetcher, &mut backoff, now + 60, 30).unwrap();
        let google_calls = fetcher
            .calls
            .borrow()
            .iter()
            .filter(|locator| locator.contains("news.google.com"))
            .count();

        assert_eq!(
            first.ok, 2,
            "the feed sources are polled, google is not one of them"
        );
        assert_eq!(second.ok, 2);
        assert_eq!(
            google_calls, 1,
            "google seeds the empty week once, not every cycle"
        );
    }
}
