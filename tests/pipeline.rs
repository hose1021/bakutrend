//! End-to-end without the network: real captured bytes -> store -> grouping -> ranking.
//! This is the test that fails if coverage, clustering or scoring regress together.

use std::cell::RefCell;
use std::collections::HashMap;

use bakutrend::cluster::Clusterer;
use bakutrend::error::FetchError;
use bakutrend::poller::{Backoff, poll_once};
use bakutrend::score::{rank, Weights};
use bakutrend::source::http::Fetcher;
use bakutrend::source::{ParseOutcome, SourceKind, SourceSpec};
use bakutrend::store::{SourceRow, Store, Window};

/// Serves captured bytes by locator, so the whole cycle runs offline and deterministically.
struct FixtureFetcher {
    bodies: HashMap<String, Vec<u8>>,
    touched: RefCell<Vec<String>>,
}

impl FixtureFetcher {
    fn new(pairs: &[(&str, &str)]) -> Self {
        let bodies = pairs
            .iter()
            .map(|(locator, path)| {
                let bytes = std::fs::read(format!("tests/fixtures/{path}")).expect("fixture present");
                (locator.to_string(), bytes)
            })
            .collect();
        Self { bodies, touched: RefCell::new(Vec::new()) }
    }
}

impl Fetcher for FixtureFetcher {
    fn fetch(&self, source: &SourceRow) -> Result<ParseOutcome, FetchError> {
        self.touched.borrow_mut().push(source.locator.clone());
        let body = self
            .bodies
            .get(&source.locator)
            .ok_or_else(|| FetchError::Http { status: 404, url: source.locator.clone() })?;
        match source.kind {
            SourceKind::Rss => {
                bakutrend::source::rss::parse(body).map_err(|source| FetchError::Parse {
                    url: source.to_string(),
                    source,
                })
            }
            SourceKind::Telegram => Ok(bakutrend::source::telegram::parse(
                &String::from_utf8_lossy(body),
            )),
            SourceKind::Google => {
                bakutrend::source::google::parse(body).map_err(|source| FetchError::Parse {
                    url: source.to_string(),
                    source,
                })
            }
        }
    }
}

fn seed_sources(store: &mut Store) -> (i64, i64, i64) {
    let rss = store
        .ensure_source(
            &SourceSpec {
                kind: SourceKind::Rss,
                outlet: "Qafqazinfo".into(),
                name: "Qafqazinfo RSS".into(),
                locator: "https://qafqazinfo.az/rss".into(),
            },
            true,
        )
        .unwrap();
    let telegram = store
        .ensure_source(
            &SourceSpec {
                kind: SourceKind::Telegram,
                outlet: "Baku Post".into(),
                name: "Baku Post Telegram".into(),
                locator: "@bakupost".into(),
            },
            true,
        )
        .unwrap();
    let apa = store
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
    (rss, telegram, apa)
}

#[test]
fn one_poll_ingests_every_source_and_produces_a_ranked_list() {
    let mut store = Store::open_in_memory().unwrap();
    seed_sources(&mut store);
    let fetcher = FixtureFetcher::new(&[
        ("https://qafqazinfo.az/rss", "qafqazinfo.rss.xml"),
        ("https://apa.az/rss", "apa.rss.xml"),
        ("@bakupost", "bakupost.tg.html"),
    ]);
    let now = chrono::Utc::now().timestamp();

    let report = poll_once(&mut store, &fetcher, &mut Backoff::new(), now, 30).unwrap();
    assert_eq!(report.failed, Vec::<(String, String)>::new());
    assert_eq!(report.ok, 3);
    assert!(report.new_items >= 50, "expected a real ingest, got {}", report.new_items);

    let (items, samples) = store.window(Window::Week, now).unwrap();
    assert!(items.len() >= 50);

    let groups = Clusterer::new(0.45).group_items(&items);
    assert!(!groups.is_empty());
    // Grouping never invents stories. Whether real headlines happen to merge is not an
    // invariant of this test — Task 6 pins that behaviour deterministically.
    assert!(groups.len() <= items.len());

    let ranked = rank(&groups, &samples, Window::Week, &Weights::default(), now);
    assert!(!ranked.is_empty());
    assert!(ranked[0].score >= ranked[ranked.len() - 1].score, "sorted best first");
    assert!(ranked.iter().all(|s| s.coverage >= 0.5), "every story has someone carrying it");
    assert!(ranked[0].outlets.len() >= 1);
}

#[test]
fn the_same_poll_run_twice_reranks_identically() {
    let mut store = Store::open_in_memory().unwrap();
    seed_sources(&mut store);
    let fetcher = FixtureFetcher::new(&[
        ("https://qafqazinfo.az/rss", "qafqazinfo.rss.xml"),
        ("@bakupost", "bakupost.tg.html"),
    ]);
    let now = chrono::Utc::now().timestamp();

    poll_once(&mut store, &fetcher, &mut Backoff::new(), now, 30).unwrap();
    let (items, samples) = store.window(Window::Week, now).unwrap();
    let first = rank(&Clusterer::new(0.45).group_items(&items), &samples, Window::Week, &Weights::default(), now);

    let mut backoff = Backoff::new();
    poll_once(&mut store, &fetcher, &mut backoff, now + 300, 30).unwrap();
    let (items2, samples2) = store.window(Window::Week, now + 300).unwrap();
    let second = rank(&Clusterer::new(0.45).group_items(&items2), &samples2, Window::Week, &Weights::default(), now + 300);

    let titles = |stories: &[bakutrend::score::ScoredStory]| {
        stories.iter().map(|s| (s.title.clone(), s.coverage)).collect::<Vec<_>>()
    };
    assert_eq!(titles(&first), titles(&second), "re-polling unchanged feeds must not change the ranking inputs");
}
