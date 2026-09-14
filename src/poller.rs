//! The poll cycle: fetch every due source, store what comes back, sample view counts,
//! prune old samples. Sources are isolated — one failure never aborts the cycle.

use std::collections::HashMap;

use crate::config::Config;
use crate::error::StoreError;
use crate::source::SourceKind;
use crate::source::http::Fetcher;
use crate::store::Store;

const BACKOFF_BASE_SECS: i64 = 60;
const BACKOFF_CAP_SECS: i64 = 1800;

/// `meta` key recording that the one-shot Google week seed completed, holding the timestamp it
/// did so. It lives in the database rather than in the poller's memory because "already seeded"
/// has to survive a restart: a run that finished its seed must not repeat it, and a run that
/// failed must not be mistaken for one that succeeded.
const GOOGLE_SEED_KEY: &str = "google_seed_completed";

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
    /// Names of sources that succeeded this cycle, so the app can retire them from
    /// its degraded set (I10). A skipped source appears in neither list.
    pub succeeded: Vec<String>,
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
                report.succeeded.push(source.name.clone());
                report.ok += 1;
            }
            Err(error) => {
                backoff.record_failure(source.id, now);
                report.failed.push((source.name.clone(), error.to_string()));
                log::warn!("source {} failed: {error}", source.name);
            }
        }
    }

    // One-shot week seed. What ends it is a recorded success, not the presence of week data:
    // the feeds the same cycle already stored are not the seed. Reading week data as the signal
    // meant a seed that failed once while the feeds succeeded was never tried again, and the
    // week window stayed as thin as the feeds alone could make it.
    //
    // A failed seed retries on the same backoff as every other source, so an unreachable Google
    // is retried at a widening interval rather than every cycle.
    if store.meta(GOOGLE_SEED_KEY)?.is_none()
        && let Some(seed) = sources.iter().find(|s| s.kind == SourceKind::Google)
        && backoff.is_due(seed.id, now)
    {
        match fetcher.fetch(seed) {
            Ok(outcome) if outcome.items.is_empty() => {
                // An answer that carried no usable article is not a completed seed. Recording it
                // as one would leave the week window permanently empty with a flag claiming it
                // was filled, and nothing would ever retry. The two reasons are named apart: a
                // feed that returned nothing and a feed whose every entry was refused are
                // different problems, and the count says which one happened.
                let reason = if outcome.skipped == 0 {
                    "the response held no articles".to_string()
                } else {
                    format!("no usable articles ({} refused)", outcome.skipped)
                };
                backoff.record_failure(seed.id, now);
                log::warn!("google backfill rejected: {reason}");
                report.failed.push((seed.name.clone(), reason));
                report.skipped += outcome.skipped;
            }
            Ok(outcome) => {
                backoff.record_ok(seed.id);
                store.upsert_items(seed.id, &outcome.items, now)?;
                // Only now, with articles actually stored, is the seed done.
                store.set_meta(GOOGLE_SEED_KEY, &now.to_string())?;
                report.skipped += outcome.skipped;
                // A seed that answered has recovered, like any other source that answers.
                report.succeeded.push(seed.name.clone());
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

/// One cycle plus the ranking it leaves behind, which is what a later run compares against.
///
/// The two are one step on purpose: a ranking stored from data older than the cycle that just
/// ran would date the movement column to a moment nothing was written at.
pub fn poll_and_record(
    store: &mut Store,
    config: &Config,
    fetcher: &dyn Fetcher,
    backoff: &mut Backoff,
    now: i64,
) -> Result<PollReport, StoreError> {
    let report = poll_once(
        store,
        fetcher,
        backoff,
        now,
        config.retention.view_sample_days,
    )?;
    crate::app::record_rankings(store, config, now)?;
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

    fn add_two_sources(store: &mut Store) {
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
    }

    fn store_with_two_sources() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        add_two_sources(&mut store);
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

    /// Serves one fresh item for every source, so a cycle leaves week data behind and the one-shot
    /// Google seed has something to store.
    struct OneItemFetcher {
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl Fetcher for OneItemFetcher {
        fn fetch(&self, source: &crate::store::SourceRow) -> Result<ParseOutcome, FetchError> {
            self.calls.borrow_mut().push(source.locator.clone());
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
                    cited_outlet: None,
                    // The seed resolves publishers from the item, so it needs one.
                    publisher: (source.kind == SourceKind::Google).then(|| "APA".to_string()),
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

    /// The seed's refused items are part of the cycle's accounting too.
    #[test]
    fn the_google_seed_reports_its_skipped_items() {
        struct SkippingSeedFetcher;
        impl Fetcher for SkippingSeedFetcher {
            fn fetch(&self, source: &crate::store::SourceRow) -> Result<ParseOutcome, FetchError> {
                if source.kind == SourceKind::Google {
                    return Ok(ParseOutcome {
                        skipped: 2,
                        ..Default::default()
                    });
                }
                Ok(ParseOutcome::default())
            }
        }

        let mut store = store_with_two_sources();
        google_source(&mut store);
        let report = poll_once(
            &mut store,
            &SkippingSeedFetcher,
            &mut Backoff::new(),
            1_700_000_000,
            30,
        )
        .unwrap();
        assert_eq!(report.skipped, 2);
    }

    /// Serves one item for every feed, and fails for Google.
    struct FeedsFeedSeedFails {
        calls: std::cell::RefCell<Vec<String>>,
        now: i64,
    }

    impl Fetcher for FeedsFeedSeedFails {
        fn fetch(&self, source: &crate::store::SourceRow) -> Result<ParseOutcome, FetchError> {
            self.calls.borrow_mut().push(source.locator.clone());
            if source.kind == SourceKind::Google {
                return Err(FetchError::Http {
                    status: 503,
                    url: source.locator.clone(),
                });
            }
            Ok(ParseOutcome {
                items: vec![crate::source::ParsedItem {
                    external_id: format!("{}-1", source.id),
                    url: format!("https://example.az/{}", source.id),
                    title: format!("Yeni xəbər {}", source.id),
                    description: None,
                    section: None,
                    published_at: self.now,
                    views: None,
                    cited: false,
                    cited_outlet: None,
                    publisher: None,
                }],
                skipped: 0,
            })
        }
    }

    /// The feeds storing news is not the seed succeeding. Reading the week window as the signal
    /// meant one Google failure during a healthy cycle ended the seed for good.
    #[test]
    fn a_failed_seed_is_retried_after_its_backoff_even_when_the_feeds_stored_news() {
        let mut store = store_with_two_sources();
        google_source(&mut store);
        let now = 1_700_000_000;
        let fetcher = FeedsFeedSeedFails {
            calls: std::cell::RefCell::new(Vec::new()),
            now,
        };
        let seeds = || {
            fetcher
                .calls
                .borrow()
                .iter()
                .filter(|locator| locator.contains("news.google.com"))
                .count()
        };
        let mut backoff = Backoff::new();

        poll_once(&mut store, &fetcher, &mut backoff, now, 30).unwrap();
        assert_eq!(seeds(), 1, "the first cycle tries the seed");
        assert!(
            !store
                .window(crate::store::Window::Week, now)
                .unwrap()
                .0
                .is_empty(),
            "the feeds filled the week in the same cycle"
        );

        poll_once(&mut store, &fetcher, &mut backoff, now + 61, 30).unwrap();
        assert_eq!(
            seeds(),
            2,
            "a seed that failed is retried once its backoff has passed"
        );
    }

    /// The completion has to survive the process, or every restart re-seeds.
    #[test]
    fn a_completed_seed_is_not_repeated_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bakutrend.sqlite");
        let now = 1_700_000_000;

        {
            let mut store = Store::open(&path).unwrap();
            add_two_sources(&mut store);
            google_source(&mut store);
            let fetcher = OneItemFetcher {
                calls: std::cell::RefCell::new(Vec::new()),
            };
            let report = poll_once(&mut store, &fetcher, &mut Backoff::new(), now, 30).unwrap();
            assert!(report.failed.is_empty(), "{:?}", report.failed);
            assert!(
                report.succeeded.iter().any(|name| name == "Google News AZ"),
                "a seed that stored an article is a source that recovered: {:?}",
                report.succeeded
            );
        }

        let second = CountingFetcher {
            calls: std::cell::RefCell::new(Vec::new()),
        };
        let mut store = Store::open(&path).unwrap();
        poll_once(&mut store, &second, &mut Backoff::new(), now + 3600, 30).unwrap();
        assert_eq!(
            second
                .calls
                .borrow()
                .iter()
                .filter(|locator| locator.contains("news.google.com"))
                .count(),
            0,
            "the next run reads the recorded completion instead of seeding again"
        );
    }

    /// An answer with no article in it is not a completed seed. Recording it as one would leave
    /// the week window empty with a flag saying it was filled, and nothing would ever retry.
    #[test]
    fn an_empty_seed_answer_does_not_complete_the_seed() {
        let mut store = store_with_two_sources();
        google_source(&mut store);
        let now = 1_700_000_000;
        let fetcher = CountingFetcher {
            calls: std::cell::RefCell::new(Vec::new()),
        };
        let seeds = || {
            fetcher
                .calls
                .borrow()
                .iter()
                .filter(|locator| locator.contains("news.google.com"))
                .count()
        };

        let mut backoff = Backoff::new();
        let first = poll_once(&mut store, &fetcher, &mut backoff, now, 30).unwrap();
        assert_eq!(seeds(), 1);
        assert_eq!(
            first
                .failed
                .iter()
                .find(|(name, _)| name == "Google News AZ")
                .map(|(_, reason)| reason.clone()),
            Some("the response held no articles".to_string()),
            "an empty answer is reported, with the reason it is not a seed"
        );
        assert!(
            store.meta(GOOGLE_SEED_KEY).unwrap().is_none(),
            "nothing was stored, so the seed is not done"
        );
        assert!(
            first.succeeded.iter().all(|name| name != "Google News AZ"),
            "an empty answer must not be reported as a source that recovered"
        );

        // The retry waits for the backoff like any other failing source.
        poll_once(&mut store, &fetcher, &mut backoff, now + 30, 30).unwrap();
        assert_eq!(seeds(), 1, "the backoff still applies");
        poll_once(&mut store, &fetcher, &mut backoff, now + 61, 30).unwrap();
        assert_eq!(seeds(), 2, "and the seed is tried again once it has passed");
    }

    /// An answer whose every entry was refused is a different problem from an answer with
    /// nothing in it, and the reason says which one happened.
    #[test]
    fn a_seed_whose_entries_were_all_refused_names_that_reason() {
        struct RejectingSeedFetcher;
        impl Fetcher for RejectingSeedFetcher {
            fn fetch(&self, source: &crate::store::SourceRow) -> Result<ParseOutcome, FetchError> {
                if source.kind == SourceKind::Google {
                    return Ok(ParseOutcome {
                        skipped: 7,
                        ..Default::default()
                    });
                }
                Ok(ParseOutcome::default())
            }
        }

        let mut store = store_with_two_sources();
        google_source(&mut store);
        let report = poll_once(
            &mut store,
            &RejectingSeedFetcher,
            &mut Backoff::new(),
            1_700_000_000,
            30,
        )
        .unwrap();

        assert_eq!(report.skipped, 7, "the refused entries are still accounted");
        assert_eq!(
            report
                .failed
                .iter()
                .find(|(name, _)| name == "Google News AZ")
                .map(|(_, reason)| reason.clone()),
            Some("no usable articles (7 refused)".to_string())
        );
        assert!(
            store.meta(GOOGLE_SEED_KEY).unwrap().is_none(),
            "refused entries are not a completed seed"
        );
    }

    /// A database written before the completion was recorded has no flag, so the seed runs once
    /// against it. It must not read its own week data as proof, and it must not lose a row.
    #[test]
    fn an_existing_database_without_a_recorded_seed_runs_it_once() {
        let mut store = store_with_two_sources();
        google_source(&mut store);
        let now = 1_700_000_000;
        let source_id = store.sources(true).unwrap()[0].id;
        store
            .upsert_items(
                source_id,
                &[crate::source::ParsedItem {
                    external_id: "old-1".into(),
                    url: "https://apa.az/old".into(),
                    title: "Köhnə xəbər".into(),
                    description: None,
                    section: None,
                    published_at: now - 60,
                    views: None,
                    cited: false,
                    cited_outlet: None,
                    publisher: None,
                }],
                now,
            )
            .unwrap();
        let stored = store.item_count().unwrap();

        let fetcher = OneItemFetcher {
            calls: std::cell::RefCell::new(Vec::new()),
        };
        let seeds = || {
            fetcher
                .calls
                .borrow()
                .iter()
                .filter(|locator| locator.contains("news.google.com"))
                .count()
        };

        poll_once(&mut store, &fetcher, &mut Backoff::new(), now, 30).unwrap();
        assert_eq!(seeds(), 1, "week data alone is not a recorded seed");
        assert_eq!(
            store.item_count().unwrap(),
            stored + 3,
            "seeding must not drop what the database already held"
        );

        poll_once(&mut store, &fetcher, &mut Backoff::new(), now + 3600, 30).unwrap();
        assert_eq!(
            seeds(),
            1,
            "and the run that recorded it does not repeat it"
        );
    }
}
