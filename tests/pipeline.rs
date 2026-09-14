//! End-to-end without the network: real captured bytes -> store -> grouping -> ranking.
//! This is the test that fails if coverage, clustering or scoring regress together.

use std::cell::RefCell;
use std::collections::HashMap;

use bakutrend::cluster::Clusterer;
use bakutrend::error::FetchError;
use bakutrend::poller::{Backoff, poll_once};
use bakutrend::score::{Weights, rank};
use bakutrend::source::http::Fetcher;
use bakutrend::source::{ParseOutcome, SourceKind, SourceSpec};
use bakutrend::store::{SourceRow, Store, Window};

/// Serves captured bytes by locator, so the whole cycle runs offline and deterministically.
struct FixtureFetcher {
    bodies: HashMap<String, Vec<u8>>,
    newest: i64,
    touched: RefCell<Vec<String>>,
}

impl FixtureFetcher {
    fn new(pairs: &[(&str, &str)]) -> Self {
        let mut bodies = HashMap::new();
        let mut newest = Vec::new();
        for (locator, path) in pairs {
            let bytes = std::fs::read(format!("tests/fixtures/{path}")).expect("fixture present");
            newest.push(
                fixture_outcome(path, &bytes)
                    .items
                    .iter()
                    .map(|item| item.published_at)
                    .max()
                    .expect("a fixture with no dated item cannot anchor the window"),
            );
            bodies.insert(locator.to_string(), bytes);
        }
        Self {
            bodies,
            newest: newest.into_iter().max().expect("at least one fixture"),
            touched: RefCell::new(Vec::new()),
        }
    }

    /// The clock this test runs on. Captured bytes carry their capture-time dates, so the window
    /// is measured from the newest of them; measured from the wall clock instead, every fixture
    /// item would fall out of the week a week after capture and the test would go red on a
    /// calendar date rather than on a regression.
    fn now(&self) -> i64 {
        self.newest + 60
    }
}

/// The two captured shapes: a `*.rss.xml` feed, or a Telegram channel page.
fn fixture_outcome(path: &str, body: &[u8]) -> ParseOutcome {
    if path.ends_with(".xml") {
        bakutrend::source::rss::parse(body).expect("fixture parses")
    } else {
        bakutrend::source::telegram::parse(&String::from_utf8_lossy(body))
    }
}

impl Fetcher for FixtureFetcher {
    fn fetch(&self, source: &SourceRow) -> Result<ParseOutcome, FetchError> {
        self.touched.borrow_mut().push(source.locator.clone());
        let body = self
            .bodies
            .get(&source.locator)
            .ok_or_else(|| FetchError::Http {
                status: 404,
                url: source.locator.clone(),
            })?;
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

/// Every fixture the seeded sources are served with. A source with no fixture 404s and drops
/// silently out of the ranking, so both cycles in this file use the same complete set.
fn all_fixtures() -> [(&'static str, &'static str); 3] {
    [
        ("https://qafqazinfo.az/rss", "qafqazinfo.rss.xml"),
        ("https://apa.az/rss", "apa.rss.xml"),
        ("@bakupost", "bakupost.tg.html"),
    ]
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
    let fetcher = FixtureFetcher::new(&all_fixtures());
    let now = fetcher.now();

    let report = poll_once(&mut store, &fetcher, &mut Backoff::new(), now, 30).unwrap();
    assert_eq!(report.failed, Vec::<(String, String)>::new());
    assert_eq!(report.ok, 3);
    assert!(
        report.new_items >= 50,
        "expected a real ingest, got {}",
        report.new_items
    );
    assert_eq!(
        report.new_items as i64,
        store.item_count().unwrap(),
        "a fresh store holds exactly what the cycle inserted"
    );

    let (items, samples) = store.window(Window::Week, now).unwrap();
    assert!(items.len() >= 50);

    let groups = Clusterer::new(0.45).group_items(&items);
    assert!(!groups.is_empty());
    // Real headlines from different outlets do merge; a no-op clusterer would return one group
    // per item. Which headlines merge is Task 6's deterministic concern, not this test's.
    assert!(
        groups.len() < items.len(),
        "the clusterer grouped something"
    );

    let ranked = rank(&groups, &samples, Window::Week, &Weights::default(), now);
    assert!(!ranked.is_empty());
    assert!(
        ranked.windows(2).all(|pair| pair[0].score >= pair[1].score),
        "sorted best first, every adjacent pair"
    );
    assert!(
        ranked[0].score > 0.0,
        "real coverage, engagement and freshness score above zero"
    );
    assert!(!ranked[0].outlets.is_empty());
    assert!(
        ranked.iter().all(|s| s.coverage >= 0.5),
        "every story has someone carrying it"
    );
}

#[test]
fn the_same_poll_run_twice_reranks_identically() {
    let mut store = Store::open_in_memory().unwrap();
    seed_sources(&mut store);
    let fetcher = FixtureFetcher::new(&all_fixtures());
    let now = fetcher.now();

    let first_report = poll_once(&mut store, &fetcher, &mut Backoff::new(), now, 30).unwrap();
    assert_eq!(first_report.failed, Vec::<(String, String)>::new());
    assert_eq!(first_report.ok, 3);
    let ingested = store.item_count().unwrap();
    assert_eq!(first_report.new_items as i64, ingested);

    // Both readings share one reference time: `now` feeds the freshness term, so ranking the
    // second read against a later clock would move every score for reasons that are not the data.
    let reading = |store: &Store| {
        let (items, samples) = store.window(Window::Week, now).unwrap();
        rank(
            &Clusterer::new(0.45).group_items(&items),
            &samples,
            Window::Week,
            &Weights::default(),
            now,
        )
    };
    let summary = |stories: &[bakutrend::score::ScoredStory]| {
        stories
            .iter()
            .map(|s| (s.title.clone(), s.coverage, s.score))
            .collect::<Vec<_>>()
    };
    let first = summary(&reading(&store));
    assert!(!first.is_empty());

    let mut backoff = Backoff::new();
    let second_report = poll_once(&mut store, &fetcher, &mut backoff, now + 300, 30).unwrap();
    assert_eq!(second_report.failed, Vec::<(String, String)>::new());
    assert_eq!(second_report.ok, 3);
    assert_eq!(second_report.new_items, 0, "unchanged feeds insert nothing");
    assert_eq!(
        store.item_count().unwrap(),
        ingested,
        "and leave the store the same size"
    );

    let second = summary(&reading(&store));
    assert_eq!(
        first, second,
        "re-polling unchanged feeds must not change the ranking inputs"
    );
}
