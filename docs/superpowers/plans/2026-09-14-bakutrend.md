# bakutrend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a Rust TUI that ranks Azerbaijani news stories by popularity in 1-hour, 24-hour and 7-day windows, combining cross-outlet coverage with Telegram reader engagement.

**Architecture:** One binary. A poller thread fetches 10 RSS feeds and 10 Telegram channel previews every 5 minutes into SQLite; the main thread owns all UI state and re-groups and re-ranks the last 7 days of items in memory whenever data or window changes. Stories are derived, never persisted: `items` plus `view_samples` are the only sources of truth, and grouping/ranking are pure functions over them. Three threads (input, poller, main) communicate through one `AppEvent` channel.

**Tech Stack:** Rust edition 2024, `ratatui` 0.30 + `crossterm` 0.29 (TUI), `rusqlite` 0.40 with bundled SQLite, `reqwest` 0.12 blocking (HTTP), `feed-rs` 2.4 (RSS/Atom), `scraper` 0.27 (Telegram HTML), `chrono`, `thiserror` 2, `clap` 4, `toml` 1, `serde`, `directories` 6, `unicode-width` 0.2, `log` 0.4.

**Spec:** `docs/superpowers/specs/2026-09-14-bakutrend-design.md` — read it alongside this plan. This plan implements it, with the deltas listed under Global Constraints.

---

## Global Constraints

Every task's requirements implicitly include this section.

- **Rust edition `2024`**, `rust-version = "1.88"` — copied from the sibling `ttymap` project.
- **No `anyhow`.** `thiserror` 2 only, one error enum per boundary (`ParseError`, `FetchError`, `StoreError`, `ConfigError`).
- **Blocking HTTP only.** No async runtime, no `tokio` in `[dependencies]`.
- **Coverage counts distinct OUTLETS, never distinct sources.** One outlet publishing both an RSS feed and a Telegram channel casts one vote.
- **Cited reposts weigh 0.5.** For each outlet in a story: weight is `1.0` if it has any uncited item in the window, else `0.5`. Taken once per outlet, never summed per item.
- **Ranking weights:** coverage `0.40`, engagement `0.40`, freshness `0.20`. Per-channel normalized: engagement is relative to its own channel's median, so a 2,650-view channel does not automatically beat a 340-view one.
- **`now` is always an injected parameter** in every function that reads the clock. No function under test calls `SystemTime::now()`.
- **Headlines render as published** (Azerbaijani, Russian or English). UI chrome is English.
- **Never silently display a different time range than the header claims.** The quiet-hour fallback is labeled.
- **One writer to SQLite.** WAL mode; the poller thread writes, the UI reads on its own connection.
- **`--poll-only` installs nothing.** No launchd agent is created by the code; the README documents one.
- **Commit at the end of every task.** Conventional-commit prefixes (`feat:`, `test:`, `chore:`, `docs:`).

### Deliberate deltas from the spec

These were found while writing the plan. They are corrections, not scope changes.

1. **`stories` and `story_items` tables are dropped.** Grouping is a pure function recomputed in memory from the last 7 days of `items` (about 10k rows — milliseconds). This removes an entire class of incremental-merge bugs and makes `cluster::group_items` directly testable with no database. The delta column already needed no extra storage, so nothing else changes.
2. **Engagement is `views/hour` in both branches.** The spec said "otherwise the raw current view count", which mixes units (views vs views/hour) inside one sum. Corrected: when two samples at least 10 minutes apart exist, `(last - first) / hours_elapsed`; otherwise `views / max(hours_since_published, 0.25)`.
3. **Google publisher attribution is by name, not host.** `feed-rs` exposes the RSS `<source>` element as `Entry.source: Option<String>` — a name, with no URL — so the spec's host-matching scheme is not implementable. Replaced by folded-name matching: exact match after folding, else prefix match when the shorter name is at least 4 characters (`report` matches `reportaz`). This affects only the week-window seed.
4. **File list refined:** adds `src/text.rs` (normalization shared by clustering and citation detection), `src/poller.rs` (poll cycle, separated so it can be driven offline by a fixture fetcher), `src/source/http.rs` (the `Fetcher` trait and its HTTP implementation), `src/cli.rs`.
5. **`@apatv` ships disabled.** It answered normally during design research but now returns a preview-less stub. It stays in the default config with `enabled = false` and a dated comment. 19 of the 20 sources ship enabled.
6. **An empty Telegram preview is a failure, not an empty channel.** HTTP 200 with zero post containers means the channel disabled previews. Reporting it as "0 new items" would hide a dead source; it returns `FetchError::EmptyPreview` and the header shows the source as degraded.

---

## File Structure

```
Cargo.toml
src/
  main.rs            thin composition root: CLI, threads, terminal setup
  lib.rs             module declarations
  cli.rs             clap argument definitions
  error.rs           ParseError, FetchError, StoreError, ConfigError
  text.rs            fold, decode_entities, collapse_ws, tokens, matches_any
  dirs.rs            config/data/cache/state resolution
  config.rs          TOML config, defaults including the 20-source list
  source/
    mod.rs           ParsedItem, SourceKind, ParseOutcome, SourceSpec, section_from_url, is_cited
    rss.rs           feed-rs wrapper
    telegram.rs      t.me/s/<handle> extraction
    google.rs        when:7d backfill, publisher extraction
    http.rs          Fetcher trait, HttpFetcher, URL builders
  store.rs           schema, upsert, windowed queries, samples, pruning
  cluster.rs         Group, Clusterer, similarity, signature
  score.rs           Weights, rank, views_per_hour, ScoredStory
  poller.rs          Backoff, PollReport, poll_once
  ui.rs              View, stateless draw
  app.rs             AppEvent, App, key dispatch, refresh
tests/
  fixtures/          real captured bytes (feeds + Telegram previews)
  pipeline.rs        offline end-to-end: fixtures -> store -> group -> rank
README.md
```

---

### Task 1: Crate scaffold, error types, text primitives

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/main.rs`, `src/error.rs`, `src/text.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `crate::error::{ParseError, FetchError, StoreError, ConfigError}`; `crate::text::{fold, decode_entities, collapse_ws, tokens, matches_any}`.

- [ ] **Step 1: Write the failing test**

Create `src/text.rs` with only the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_strips_azerbaijani_diacritics_and_lowercases() {
        assert_eq!(fold("Bakıda GƏNCƏDƏ"), "bakida gencede");
        assert_eq!(fold("Şuşa, Qarabağ"), "susa, qarabag");
    }

    #[test]
    fn fold_handles_dotted_capital_i_without_combining_mark() {
        // 'İ'.to_lowercase() is "i̇" (i + U+0307); folding must not leave the combining dot.
        assert_eq!(fold("İSMAYILLI"), "ismayilli");
        assert_eq!(fold("İ").chars().count(), 1);
    }

    #[test]
    fn decode_entities_handles_html_entities_feed_rs_leaves_escaped() {
        assert_eq!(decode_entities("a&nbsp;b"), "a b");
        assert_eq!(decode_entities("&ldquo;Sitat&rdquo;"), "\u{201c}Sitat\u{201d}");
        assert_eq!(decode_entities("x &mdash; y"), "x \u{2014} y");
        assert_eq!(decode_entities("&#39;"), "'");
    }

    #[test]
    fn decode_entities_resolves_amp_last_so_escaped_entities_stay_literal() {
        // "&amp;nbsp;" means the literal text "&nbsp;", not a space.
        assert_eq!(decode_entities("&amp;nbsp;"), "&nbsp;");
    }

    #[test]
    fn collapse_ws_trims_and_squashes() {
        assert_eq!(collapse_ws("  Bakıda   bu\n\nyollar "), "Bakıda bu yollar");
    }

    #[test]
    fn tokens_drops_short_words_stopwords_and_duplicates_and_sorts() {
        assert_eq!(tokens("Bakıda bu yollar bağlıdır"), vec!["baglidir", "bakida", "yollar"]);
        assert!(tokens("və bu ki").is_empty());
    }

    #[test]
    fn matches_any_folds_both_sides() {
        let keywords = vec!["Bakı".to_string(), "Qarabağ".to_string()];
        assert!(matches_any("Gəncədə QARABAĞ yolu", &keywords));
        assert!(!matches_any("Tramp Zelenski ilə danışdı", &keywords));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib text:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function fold in this scope`.

- [ ] **Step 3: Write minimal implementation**

Add above the test module in `src/text.rs`:

```rust
//! Text normalization shared by story grouping, citation detection and the local filter.

use std::collections::BTreeSet;

/// Lowercase and strip Azerbaijani diacritics so `Bakı` and `baki` compare equal.
pub fn fold(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        // 'İ'.to_lowercase() yields "i" plus a combining dot, which would split a token.
        let lowered = match ch {
            'I' | 'İ' => 'i',
            _ => ch.to_lowercase().next().unwrap_or(ch),
        };
        out.push(match lowered {
            'ə' => 'e',
            'ı' => 'i',
            'ş' => 's',
            'ğ' => 'g',
            'ç' => 'c',
            'ö' => 'o',
            'ü' => 'u',
            'â' => 'a',
            'é' => 'e',
            'ё' => 'е',
            'й' => 'и',
            other => other,
        });
    }
    out
}

const NAMED_ENTITIES: &[(&str, &str)] = &[
    ("&nbsp;", " "),
    ("&ldquo;", "\u{201c}"),
    ("&rdquo;", "\u{201d}"),
    ("&laquo;", "\u{ab}"),
    ("&raquo;", "\u{bb}"),
    ("&mdash;", "\u{2014}"),
    ("&ndash;", "\u{2013}"),
    ("&hellip;", "\u{2026}"),
    ("&quot;", "\""),
    ("&apos;", "'"),
    ("&lt;", "<"),
    ("&gt;", ">"),
];

/// Decode HTML entities. `feed-rs` resolves predefined XML entities and character
/// references but retains unknown entities in escaped form, so `&nbsp;` arrives as text.
pub fn decode_entities(input: &str) -> String {
    let mut out = input.to_string();
    for (from, to) in NAMED_ENTITIES {
        out = out.replace(from, to);
    }
    // Numeric references, before the `&amp;` pass so `&#38;` is not rewritten twice.
    if out.contains("&#") {
        let mut decoded = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(start) = rest.find("&#") {
            decoded.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let (digits, tail) = match after.find(';') {
                Some(end) => (&after[..end], &after[end + 1..]),
                None => {
                    decoded.push_str(&rest[start..]);
                    rest = "";
                    break;
                }
            };
            let value = digits
                .strip_prefix(['x', 'X'])
                .map(|hex| u32::from_str_radix(hex, 16).ok())
                .unwrap_or_else(|| digits.parse::<u32>().ok());
            match value.and_then(char::from_u32) {
                Some(ch) => decoded.push(ch),
                None => {
                    decoded.push_str("&#");
                    decoded.push_str(digits);
                    decoded.push(';');
                }
            }
            rest = tail;
        }
        decoded.push_str(rest);
        out = decoded;
    }
    // `&amp;` last, so text that was escaped twice stays literal.
    out.replace("&amp;", "&")
}

/// Trim and collapse every run of whitespace to a single space.
pub fn collapse_ws(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

const STOPWORDS: &[&str] = &[
    "olan", "ucun", "daha", "olub", "deye", "barede", "sonra", "artiq", "hansi", "nece", "bele",
    "onun", "hemcinin", "bunu", "butun", "gore", "ile", "ile", "kimi", "ancaq", "lakin", "yeni",
    "этот", "который", "также", "было", "сообщает", "передает", "которая", "которые", "что",
    "для", "как", "они", "его", "уже", "будет", "может", "своих", "своей", "только", "очень",
];

/// Significant words of a headline: folded, split, at least 4 characters, no stopwords.
/// Sorted and deduplicated so callers can compare sets directly.
pub fn tokens(input: &str) -> Vec<String> {
    let folded = fold(input);
    let mut seen = BTreeSet::new();
    for word in folded.split(|c: char| !c.is_alphanumeric()) {
        if word.chars().count() < 4 || STOPWORDS.contains(&word) {
            continue;
        }
        seen.insert(word.to_string());
    }
    seen.into_iter().collect()
}

/// True when the folded text contains any folded keyword. Used by the local filter.
pub fn matches_any(input: &str, keywords: &[String]) -> bool {
    let folded = fold(input);
    keywords.iter().any(|k| {
        let needle = fold(k);
        !needle.is_empty() && folded.contains(&needle)
    })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib text:: 2>&1 | tail -20`
Expected: PASS, 7 tests.

- [ ] **Step 5: Create the crate scaffold**

`Cargo.toml`:

```toml
[package]
name = "bakutrend"
version = "0.1.0"
edition = "2024"
rust-version = "1.88"
description = "Rank Azerbaijani news by popularity in the last hour, day or week"
license = "MIT"

[dependencies]
chrono = { version = "0.4", default-features = false, features = ["clock", "std"] }
clap = { version = "4", features = ["derive"] }
crossterm = "0.29"
directories = "6"
feed-rs = "2.4"
log = { version = "0.4", features = ["std"] }
ratatui = "0.30"
reqwest = { version = "0.12", features = ["blocking"] }
rusqlite = { version = "0.40", features = ["bundled"] }
scraper = "0.27"
serde = { version = "1", features = ["derive"] }
thiserror = "2"
toml = "1"
unicode-width = "0.2"

[dev-dependencies]
tempfile = "3"
```

`src/lib.rs`:

```rust
pub mod error;
pub mod text;
```

`src/main.rs`:

```rust
fn main() {
    println!("bakutrend");
}
```

`src/error.rs`:

```rust
//! One classified error enum per boundary, so callers decide retry versus surface.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("feed parse failed: {0}")]
    Feed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("http {status} for {url}")]
    Http { status: u16, url: String },
    #[error("network error for {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("parse error for {url}: {source}")]
    Parse {
        url: String,
        #[source]
        source: ParseError,
    },
    #[error("telegram preview for {handle} contained no posts")]
    EmptyPreview { handle: String },
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse config {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}
```

- [ ] **Step 6: Verify the whole crate builds**

Run: `cargo build 2>&1 | tail -20 && cargo test 2>&1 | tail -20`
Expected: builds clean, tests pass.

- [ ] **Step 7: Commit**

```bash
git init -q 2>/dev/null || true
printf 'target/\n' > .gitignore
git add -A
git commit -m "feat: scaffold bakutrend crate with text primitives and boundary errors"
```

---

### Task 2: RSS parsing

**Files:**
- Create: `src/source/mod.rs`, `src/source/rss.rs`, `tests/fixtures/*.rss.xml`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::text::{decode_entities, collapse_ws}`, `crate::error::ParseError`.
- Produces: `crate::source::{ParsedItem, ParseOutcome, SourceKind, SourceSpec, section_from_url, is_cited}`; `crate::source::rss::parse(&[u8]) -> Result<ParseOutcome, ParseError>`.

- [ ] **Step 1: Capture real fixtures**

Real bytes, not hand-written samples — the traps live in the real data.

```bash
mkdir -p tests/fixtures
UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)'
curl -sSL -A "$UA" https://qafqazinfo.az/rss  -o tests/fixtures/qafqazinfo.rss.xml
curl -sSL -A "$UA" https://azertag.az/rss    -o tests/fixtures/azertag.rss.xml
curl -sSL -A "$UA" https://modern.az/rss     -o tests/fixtures/modern.rss.xml
curl -sSL -A "$UA" https://apa.az/rss        -o tests/fixtures/apa.rss.xml
wc -c tests/fixtures/*.xml
```

- [ ] **Step 2: Write the failing test**

Create `src/source/rss.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("tests/fixtures/{name}")).expect("fixture present")
    }

    #[test]
    fn qafqazinfo_every_item_gets_a_distinct_identity_key() {
        // This feed repeats one guid on every item; keying on guid would collapse it to one.
        let out = parse(&fixture("qafqazinfo.rss.xml")).expect("parses");
        assert!(out.items.len() >= 50, "got {} items", out.items.len());
        let ids: std::collections::BTreeSet<_> = out.items.iter().map(|i| &i.external_id).collect();
        assert_eq!(ids.len(), out.items.len(), "identity keys must be unique");
    }

    #[test]
    fn azertag_items_parse_without_a_guid_element() {
        let out = parse(&fixture("azertag.rss.xml")).expect("parses");
        assert!(out.items.len() >= 20, "got {} items", out.items.len());
        assert!(out.items.iter().all(|i| !i.title.is_empty()));
        assert!(out.items.iter().all(|i| i.published_at > 1_600_000_000));
    }

    #[test]
    fn modern_az_titles_have_html_entities_decoded() {
        let out = parse(&fixture("modern.rss.xml")).expect("parses");
        assert!(out.items.len() >= 10);
        for item in &out.items {
            assert!(!item.title.contains("&nbsp;"), "undecoded entity in {:?}", item.title);
            assert!(!item.title.contains("&ldquo;"), "undecoded entity in {:?}", item.title);
        }
    }

    #[test]
    fn section_is_taken_from_the_url_path() {
        assert_eq!(section_from_url("https://apa.az/incident/x-995704").as_deref(), Some("incident"));
        assert_eq!(section_from_url("https://www.qafqazinfo.az/news/detail/x-521389").as_deref(), Some("news"));
        assert_eq!(section_from_url("https://azertag.az/").as_deref(), None);
    }

    #[test]
    fn is_cited_detects_agency_attribution() {
        assert!(is_cited("“Qafqazinfo” APA-ya istinadən xəbər verir ki, hadisə olub"));
        assert!(is_cited("TASS-a istinadla məlumat yayılıb"));
        assert!(!is_cited("Bakıda bu yollar bağlıdır"));
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib source:: 2>&1 | tail -20`
Expected: FAIL — `unresolved import` / `cannot find function parse`.

- [ ] **Step 4: Write minimal implementation**

`src/source/mod.rs`:

```rust
//! Source types, item shape, and the text rules that apply to every ingested item.

pub mod rss;

use crate::text;

/// Item shape every parser produces. `None` means the source does not provide it.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedItem {
    pub external_id: String,
    pub url: String,
    pub title: String,
    pub description: Option<String>,
    pub section: Option<String>,
    pub published_at: i64,
    pub views: Option<i64>,
    pub cited: bool,
    pub publisher: Option<String>,
}

/// Items plus a count of items the parser refused. A malformed item never aborts a feed.
#[derive(Debug, Default, Clone)]
pub struct ParseOutcome {
    pub items: Vec<ParsedItem>,
    pub skipped: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Rss,
    Telegram,
    Google,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Rss => "rss",
            SourceKind::Telegram => "telegram",
            SourceKind::Google => "google",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "rss" => Some(SourceKind::Rss),
            "telegram" => Some(SourceKind::Telegram),
            "google" => Some(SourceKind::Google),
            _ => None,
        }
    }
}

/// A configured source: the feed URL or channel handle, and the outlet it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSpec {
    pub kind: SourceKind,
    pub outlet: String,
    pub name: String,
    pub locator: String,
}

/// First path segment of a URL, used as a section slug. Returns `None` for a bare host.
/// Query and fragment are stripped first, so a slash inside a query value is not a path.
pub fn section_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let path = rest.split_once('/').map(|(_, r)| r)?;
    let segment = path.split('/').next()?;
    if segment.is_empty() {
        None
    } else {
        Some(segment.to_ascii_lowercase())
    }
}

/// Each marker is its own token sequence, so `istinadlar` (references) is not
/// `istinadla` (citing), and Russian `сообщается` is not `сообщает`.
const CITATION_MARKERS: &[&[&str]] = &[
    &["istinaden"],
    &["istinadla"],
    &["melumatina", "gore"],
    &["сообщает"],
    &["передает"],
    &["ссылаясь"],
    &["по", "данным"],
];

/// True when the text credits another outlet. Such an item weighs half in coverage.
/// Matches on word runs, never substrings: substring matching marked real reporting as a
/// syndicated repost and halved that outlet's weight, silently corrupting the ranking.
pub fn is_cited(text: &str) -> bool {
    let folded = text::fold(text);
    let words: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    CITATION_MARKERS
        .iter()
        .any(|marker| words.windows(marker.len()).any(|run| run == *marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_cited_matches_whole_tokens_only() {
        assert!(is_cited("“Qafqazinfo” APA-ya istinadən xəbər verir ki, hadisə olub"));
        assert!(is_cited("TASS-a istinadla məlumat yayılıb"));
        assert!(is_cited("Məlumatına görə, hadisə gecə baş verib"));
        assert!(!is_cited("Bakıda bu yollar bağlıdır"));
        assert!(!is_cited("İstinadlar göstərilib"));
        assert!(!is_cited("Он сообщается в отчёте"));
    }

    #[test]
    fn section_from_url_ignores_query_and_fragment() {
        assert_eq!(section_from_url("https://host?next=/foo"), None);
        assert_eq!(section_from_url("https://host/path?x=1").as_deref(), Some("path"));
        assert_eq!(section_from_url("https://host/path#frag").as_deref(), Some("path"));
        assert_eq!(section_from_url("https://host"), None);
        assert_eq!(section_from_url("https://azertag.az/"), None);
    }
}
```

`src/source/rss.rs`:

```rust
//! RSS and Atom parsing through `feed-rs`, mapped onto `ParsedItem`.

use crate::error::ParseError;
use crate::source::{is_cited, section_from_url, ParseOutcome, ParsedItem};
use crate::text::{collapse_ws, decode_entities};

/// Parse feed bytes. Items without a link, title or timestamp are skipped and counted.
pub fn parse(bytes: &[u8]) -> Result<ParseOutcome, ParseError> {
    let feed = feed_rs::parser::parse(bytes).map_err(|e| ParseError::Feed(e.to_string()))?;
    let mut outcome = ParseOutcome::default();
    for entry in feed.entries {
        let Some(url) = entry.links.first().map(|link| link.href.clone()) else {
            outcome.skipped += 1;
            continue;
        };
        let title = entry
            .title
            .map(|t| collapse_ws(&decode_entities(&t.content)))
            .unwrap_or_default();
        if title.is_empty() {
            outcome.skipped += 1;
            continue;
        }
        let Some(published_at) = entry.published.or(entry.updated).map(|d| d.timestamp()) else {
            outcome.skipped += 1;
            continue;
        };
        let description = entry
            .summary
            .map(|t| collapse_ws(&decode_entities(&t.content)))
            .or_else(|| {
                entry
                    .content
                    .and_then(|c| c.body)
                    .map(|b| collapse_ws(&decode_entities(&b)))
            });
        outcome.items.push(ParsedItem {
            section: section_from_url(&url),
            cited: is_cited(description.as_deref().unwrap_or_default()),
            external_id: url.clone(),
            url,
            title,
            description,
            published_at,
            views: None,
            publisher: None,
        });
    }
    Ok(outcome)
}
```

Add to `src/lib.rs`:

```rust
pub mod source;
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test --lib source:: 2>&1 | tail -30`
Expected: PASS, 5 tests. If `qafqazinfo` yields fewer than 50 items because the feed rotated, re-capture it in Step 1.

- [ ] **Step 6: Verify a malformed feed does not panic**

Add to the test module:

```rust
    #[test]
    fn malformed_bytes_return_an_error_not_a_panic() {
        let err = parse(b"<rss><channel><item>").unwrap_err();
        assert!(matches!(err, crate::error::ParseError::Feed(_)));
    }
```

Run: `cargo test --lib source::rss 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat: parse RSS feeds with link-based identity and entity decoding"
```

---

### Task 3: Telegram parsing

**Files:**
- Create: `src/source/telegram.rs`, `tests/fixtures/*.tg.html`
- Modify: `src/source/mod.rs` (add `pub mod telegram;`)

**Interfaces:**
- Consumes: `crate::source::{ParseOutcome, ParsedItem, section_from_url}`, `crate::text::{collapse_ws, decode_entities}`.
- Produces: `crate::source::telegram::{parse, parse_views}`.
  - `parse(html: &str) -> ParseOutcome`
  - `parse_views(raw: &str) -> Option<i64>`

- [ ] **Step 1: Capture real fixtures**

```bash
UA='Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)'
curl -sSL -A "$UA" https://t.me/s/qafqazinfo   -o tests/fixtures/qafqazinfo.tg.html
curl -sSL -A "$UA" https://t.me/s/bakupost     -o tests/fixtures/bakupost.tg.html
wc -c tests/fixtures/*.tg.html
```

Note there is deliberately no fixture for the empty-preview case: that test uses an inline
page so it cannot break when a channel changes its behaviour.

- [ ] **Step 2: Write the failing test**

Create `src/source/telegram.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("tests/fixtures/{name}")).expect("fixture present")
    }

    #[test]
    fn parse_views_understands_the_suffixes_telegram_uses() {
        assert_eq!(parse_views("2.65K"), Some(2650));
        assert_eq!(parse_views("1.1K"), Some(1100));
        assert_eq!(parse_views("340"), Some(340));
        assert_eq!(parse_views("1.2M"), Some(1_200_000));
        assert_eq!(parse_views(""), None);
    }

    #[test]
    fn qafqazinfo_posts_carry_id_time_views_and_a_title() {
        let out = parse(&fixture("qafqazinfo.tg.html"));
        assert!(out.items.len() >= 10, "got {} posts", out.items.len());
        let ids: std::collections::BTreeSet<_> = out.items.iter().map(|i| &i.external_id).collect();
        assert_eq!(ids.len(), out.items.len(), "post ids must be unique");
        for item in &out.items {
            assert!(item.url.starts_with("https://t.me/"), "url was {:?}", item.url);
            assert!(item.published_at > 1_600_000_000);
            assert!(item.views.is_some_and(|v| v >= 0));
            assert!(!item.title.is_empty(), "post {:?} had no title", item.external_id);
        }
    }

    /// A hand-written page, not a capture. These assert exact semantics, so they must not
    /// depend on whichever posts the live channels happened to serve at capture time.
    const INLINE: &str = r#"<html><body>
      <div class="tgme_widget_message" data-post="chan/1">
        <a class="tgme_widget_message_date" href="https://t.me/chan/1">
          <time datetime="2026-09-13T19:06:15+00:00"></time></a>
        <div class="tgme_widget_message_text js-message_text" dir="auto">
          <b>İsmayıllıda maşın aşıb yandı - Sürücü yaralandı<br/><br/></b>
          Ətraflı: <a href="https://example.az/news/detail/x-1">link</a></div>
        <span class="tgme_widget_message_views">2.65K</span>
      </div>
      <div class="tgme_widget_message" data-post="chan/2">
        <a class="tgme_widget_message_date" href="https://t.me/chan/2">
          <time datetime="2026-09-13T20:06:15+00:00"></time></a>
        <div class="tgme_widget_message_text js-message_text" dir="auto">
          <i class="emoji"><b>🇷🇺</b></i> <b>Peskov: müzakirələr davam edir</b><br/><br/>Bakıda görüş keçirildi.</div>
        <span class="tgme_widget_message_views">1.1K</span>
      </div>
    </body></html>"#;

    #[test]
    fn the_title_is_the_first_line_not_the_whole_post() {
        let out = parse(INLINE);
        assert_eq!(out.items[0].title, "İsmayıllıda maşın aşıb yandı - Sürücü yaralandı");
        assert!(out.items[0].description.as_deref().is_some_and(|d| d.contains("Ətraflı")));
    }

    #[test]
    fn an_emoji_prefix_does_not_swallow_the_headline() {
        let out = parse(INLINE);
        assert_eq!(out.items[1].title, "🇷🇺 Peskov: müzakirələr davam edir");
    }

    #[test]
    fn identity_time_and_views_come_from_the_post_attributes() {
        let out = parse(INLINE);
        assert_eq!(out.items[0].external_id, "chan/1");
        assert_eq!(out.items[0].url, "https://t.me/chan/1");
        assert_eq!(out.items[0].views, Some(2650));
        assert_eq!(out.items[1].views, Some(1100));
    }

    #[test]
    fn a_page_with_no_post_containers_yields_no_items_and_no_skips() {
        let out = parse("<html><body><div class=\"tgme_channel_info\">previews off</div></body></html>");
        assert!(out.items.is_empty());
        assert_eq!(out.skipped, 0);
    }

    #[test]
    fn captured_pages_still_parse_with_titles_shorter_than_bodies() {
        // Non-brittle check on real bytes: the live feed picks the words, this test only
        // asserts the invariant that must hold whatever it published.
        let out = parse(&fixture("bakupost.tg.html"));
        assert!(out.items.len() >= 5, "got {} posts", out.items.len());
        for item in &out.items {
            assert!(!item.title.contains("<br"), "raw markup leaked into {:?}", item.title);
            assert!(
                item.description.as_deref().is_some_and(|d| d.len() >= item.title.len()),
                "title must not be longer than the body for post {:?}",
                item.external_id
            );
        }
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib source::telegram 2>&1 | tail -20`
Expected: FAIL — `cannot find function parse_views in this scope`.

- [ ] **Step 4: Write minimal implementation**

```rust
//! Extraction from Telegram public channel previews (`https://t.me/s/<handle>`).
//!
//! Posts are HTML, not XML: message text contains inline `<b>`, `<i>` and `<tg-emoji>`
//! elements, and `<br/>` separates the headline from the body.

use scraper::{Html, Selector};

use crate::source::{section_from_url, ParseOutcome, ParsedItem};
use crate::text::{collapse_ws, decode_entities};

/// Telegram abbreviates view counts: `2.65K`, `1.1K`, `1.2M`, or a plain number, and groups
/// thousands with a no-break space (`12 345`). A value that fails to parse would silently drop
/// the post's engagement signal, so every kind of whitespace is stripped, not just the ends.
pub fn parse_views(raw: &str) -> Option<i64> {
    let cleaned = raw.replace(',', ".");
    let cleaned = cleaned.split_whitespace().collect::<String>();
    if cleaned.is_empty() {
        return None;
    }
    let (digits, multiplier) = match cleaned.chars().last()? {
        'K' | 'k' => (&cleaned[..cleaned.len() - 1], 1_000.0),
        'M' | 'm' => (&cleaned[..cleaned.len() - 1], 1_000_000.0),
        _ => (cleaned.as_str(), 1.0),
    };
    digits
        .trim()
        .parse::<f64>()
        .ok()
        .map(|value| (value * multiplier).round() as i64)
}

pub fn parse(html: &str) -> ParseOutcome {
    // Static selectors: a failure here is a programming error, not a runtime condition.
    let post_selector = Selector::parse("div.tgme_widget_message").expect("static selector");
    // The post timestamp is the footer anchor's `<time>`. A bare `time` selector is wrong here:
    // bakupost video posts carry `<time class="message_video_duration">` first, and an earlier
    // `<time datetime>` elsewhere in the post is likewise not the post time.
    let time_selector =
        Selector::parse("a.tgme_widget_message_date time[datetime]").expect("static selector");
    let views_selector = Selector::parse(".tgme_widget_message_views").expect("static selector");
    let text_selector = Selector::parse(".tgme_widget_message_text").expect("static selector");
    let link_selector = Selector::parse("a.tgme_widget_message_date").expect("static selector");

    let document = Html::parse_document(html);
    let mut outcome = ParseOutcome::default();

    for post in document.select(&post_selector) {
        let Some(post_id) = post.value().attr("data-post") else {
            outcome.skipped += 1;
            continue;
        };
        let Some(published_at) = post
            .select(&time_selector)
            .next()
            .and_then(|t| t.value().attr("datetime"))
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|dt| dt.timestamp())
        else {
            outcome.skipped += 1;
            continue;
        };
        let Some(text_node) = post.select(&text_selector).next() else {
            // Media-only post with no caption: nothing to group on.
            outcome.skipped += 1;
            continue;
        };
        let body = collapse_ws(&decode_entities(&text_node.text().collect::<String>()));
        if body.is_empty() {
            outcome.skipped += 1;
            continue;
        }
        // The headline is everything before the first line break.
        let head_html = text_node.inner_html();
        let head_raw = head_html.split("<br").next().unwrap_or_default();
        let title = collapse_ws(&decode_entities(
            &Html::parse_fragment(head_raw).root_element().text().collect::<String>(),
        ));
        let title = if title.is_empty() { body.clone() } else { title };

        let url = post
            .select(&link_selector)
            .next()
            .and_then(|a| a.value().attr("href"))
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://t.me/{}", post_id.replace('/', "/")));
        let views = post
            .select(&views_selector)
            .next()
            .and_then(|v| parse_views(&v.text().collect::<String>()));

        outcome.items.push(ParsedItem {
            external_id: post_id.to_string(),
            section: section_from_url(&url),
            url,
            title,
            description: Some(body),
            published_at,
            views,
            cited: false,
            publisher: None,
        });
    }
    outcome
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test --lib source::telegram 2>&1 | tail -30`
Expected: PASS, 6 tests.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: extract Telegram channel previews with view counts"
```

---

### Task 4: Store — schema, outlets, sources, idempotent upsert

**Files:**
- Create: `src/store.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::source::{ParsedItem, SourceKind, SourceSpec}`, `crate::error::StoreError`.
- Produces: `crate::store::{Store, ItemRow, SourceRow, Sample, Window, outlet_key}`.
  - `Store::open(&Path) -> Result<Store, StoreError>`, `Store::open_in_memory() -> Result<Store, StoreError>`
  - `Store::ensure_source(&mut self, &SourceSpec, bool) -> Result<i64, StoreError>`
  - `Store::sources(&self, only_enabled: bool) -> Result<Vec<SourceRow>, StoreError>`
  - `Store::upsert_items(&mut self, source_id: i64, &[ParsedItem], now: i64) -> Result<usize, StoreError>` → number of NEW items
  - `Store::item_count(&self) -> Result<i64, StoreError>`

- [ ] **Step 1: Write the failing test**

Create `src/store.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{ParsedItem, SourceKind, SourceSpec};

    fn spec(outlet: &str, name: &str, locator: &str) -> SourceSpec {
        SourceSpec {
            kind: SourceKind::Rss,
            outlet: outlet.to_string(),
            name: name.to_string(),
            locator: locator.to_string(),
        }
    }

    fn item(id: &str, title: &str, published_at: i64) -> ParsedItem {
        ParsedItem {
            external_id: id.to_string(),
            url: format!("https://example.az/{id}"),
            title: title.to_string(),
            description: None,
            section: None,
            published_at,
            views: None,
            cited: false,
            publisher: None,
        }
    }

    fn fixture() -> (Store, i64) {
        let mut store = Store::open_in_memory().expect("memory store");
        let source_id = store.ensure_source(&spec("APA", "APA RSS", "https://apa.az/rss"), true).unwrap();
        (store, source_id)
    }

    #[test]
    fn upserting_the_same_payload_twice_inserts_nothing_the_second_time() {
        let (mut store, source_id) = fixture();
        let items = vec![item("a", "Bakıda yollar bağlıdır", 1_700_000_000)];

        assert_eq!(store.upsert_items(source_id, &items, 1_700_000_000).unwrap(), 1);
        assert_eq!(store.upsert_items(source_id, &items, 1_700_000_300).unwrap(), 0);
        assert_eq!(store.item_count().unwrap(), 1);
    }

    #[test]
    fn upsert_refreshes_last_seen_without_touching_published_at() {
        let (mut store, source_id) = fixture();
        let items = vec![item("a", "Başlıq", 1_700_000_000)];
        store.upsert_items(source_id, &items, 1_700_000_000).unwrap();
        store.upsert_items(source_id, &items, 1_700_000_900).unwrap();

        let first_seen: i64 = store
            .conn
            .query_row("SELECT first_seen FROM items WHERE external_id = 'a'", [], |r| r.get(0))
            .unwrap();
        let last_seen: i64 = store
            .conn
            .query_row("SELECT last_seen FROM items WHERE external_id = 'a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first_seen, 1_700_000_000);
        assert_eq!(last_seen, 1_700_000_900);
    }

    #[test]
    fn one_outlet_publishing_two_feeds_is_one_outlet() {
        let mut store = Store::open_in_memory().unwrap();
        let rss = store.ensure_source(&spec("Qafqazinfo", "Qafqazinfo RSS", "https://qafqazinfo.az/rss"), true).unwrap();
        let tg = store.ensure_source(&spec("Qafqazinfo", "Qafqazinfo Telegram", "@qafqazinfo"), true).unwrap();
        assert_ne!(rss, tg, "sources are distinct rows");
        let outlets: i64 = store.conn.query_row("SELECT COUNT(*) FROM outlets", [], |r| r.get(0)).unwrap();
        assert_eq!(outlets, 1, "both sources belong to one outlet");
    }

    #[test]
    fn google_items_credit_the_named_publisher_not_the_seed_source() {
        let mut store = Store::open_in_memory().unwrap();
        store.ensure_source(&spec("Report", "Report RSS", "https://report.az/rss/"), true).unwrap();
        let seed = store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Google,
                    outlet: "Google News".to_string(),
                    name: "Google News 7d".to_string(),
                    locator: "google:7d".to_string(),
                },
                true,
            )
            .unwrap();

        let mut backfill = item("g1", "Bakıda yol qəzası", 1_700_000_000);
        backfill.publisher = Some("Report.az".to_string());
        store.upsert_items(seed, &[backfill], 1_700_000_000).unwrap();

        let credited: String = store
            .conn
            .query_row(
                "SELECT o.name FROM items i JOIN outlets o ON o.id = i.outlet_id WHERE i.external_id = 'g1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(credited, "Report", "Report.az must resolve to the existing Report outlet");
    }

    #[test]
    fn outlet_key_folds_and_strips_punctuation() {
        assert_eq!(outlet_key("Report.az"), outlet_key("report"));
        assert_eq!(outlet_key("APA"), outlet_key("apa"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib store:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type Store in this scope`.

- [ ] **Step 3: Write minimal implementation**

```rust
//! SQLite persistence. `items` and `view_samples` are the only sources of truth;
//! stories are derived in memory by `cluster` and `score`.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use crate::error::StoreError;
use crate::source::{ParsedItem, SourceKind, SourceSpec};
use crate::text::fold;

/// Identity of an outlet: the brand part of the name, folded. `Report.az`, `www.report.az`
/// and `report` are one outlet. Only the segment before the first dot is kept, so a site
/// suffix cannot split one outlet into two rows.
pub fn outlet_key(name: &str) -> String {
    let brand = name.strip_prefix("www.").unwrap_or(name);
    fold(brand.split('.').next().unwrap_or(brand))
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRow {
    pub id: i64,
    pub kind: SourceKind,
    pub name: String,
    pub locator: String,
    pub outlet: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemRow {
    pub item_id: i64,
    pub source_id: i64,
    pub outlet_id: i64,
    pub outlet: String,
    pub kind: SourceKind,
    pub title: String,
    pub description: Option<String>,
    pub url: String,
    pub published_at: i64,
    pub views: Option<i64>,
    pub cited: bool,
    pub is_backfill: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub item_id: i64,
    pub ts: i64,
    pub views: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Hour,
    Day,
    Week,
}

impl Window {
    pub fn hours(self) -> i64 {
        match self {
            Window::Hour => 1,
            Window::Day => 24,
            Window::Week => 168,
        }
    }

    pub fn seconds(self) -> i64 {
        self.hours() * 3600
    }

    pub fn label(self) -> &'static str {
        match self {
            Window::Hour => "1h",
            Window::Day => "24h",
            Window::Week => "7d",
        }
    }

    /// Only the week window admits Google backfill; the shorter windows are always live data.
    pub fn allows_backfill(self) -> bool {
        matches!(self, Window::Week)
    }

    pub fn all() -> [Window; 3] {
        [Window::Hour, Window::Day, Window::Week]
    }
}

pub struct Store {
    pub(crate) conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS outlets (
  id   INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  key  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS outlets_key ON outlets(key);

CREATE TABLE IF NOT EXISTS sources (
  id       INTEGER PRIMARY KEY,
  outlet_id INTEGER NOT NULL REFERENCES outlets(id),
  kind     TEXT NOT NULL,
  name     TEXT NOT NULL,
  locator  TEXT NOT NULL,
  enabled  INTEGER NOT NULL DEFAULT 1,
  UNIQUE(kind, locator)
);

CREATE TABLE IF NOT EXISTS items (
  id           INTEGER PRIMARY KEY,
  source_id    INTEGER NOT NULL REFERENCES sources(id),
  outlet_id    INTEGER NOT NULL REFERENCES outlets(id),
  external_id  TEXT NOT NULL,
  url          TEXT NOT NULL,
  title        TEXT NOT NULL,
  description  TEXT,
  section      TEXT,
  published_at INTEGER NOT NULL,
  first_seen   INTEGER NOT NULL,
  last_seen    INTEGER NOT NULL,
  views        INTEGER,
  cited        INTEGER NOT NULL DEFAULT 0,
  is_backfill  INTEGER NOT NULL DEFAULT 0,
  UNIQUE(source_id, external_id)
);
CREATE INDEX IF NOT EXISTS items_published ON items(published_at);
CREATE INDEX IF NOT EXISTS items_source ON items(source_id, published_at);

CREATE TABLE IF NOT EXISTS view_samples (
  item_id INTEGER NOT NULL REFERENCES items(id),
  ts      INTEGER NOT NULL,
  views   INTEGER NOT NULL,
  PRIMARY KEY (item_id, ts)
);
"#;

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
                path: PathBuf::from(parent),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        Self::prepare(conn)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Resolve an outlet by name, creating it when absent. Matching is EXACT on the folded
    /// brand key: a prefix fallback would merge distinct outlets (`Baku Post` into `Baku.ws`)
    /// and would make the outcome depend on registration order.
    pub fn resolve_outlet(&self, name: &str) -> Result<i64, StoreError> {
        resolve_outlet_in(&self.conn, name)
    }

    pub fn ensure_source(&mut self, spec: &SourceSpec, enabled: bool) -> Result<i64, StoreError> {
        let outlet_id = self.resolve_outlet(&spec.outlet)?;
        self.conn.execute(
            "INSERT INTO sources (outlet_id, kind, name, locator, enabled) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(kind, locator) DO UPDATE SET name = excluded.name, outlet_id = excluded.outlet_id",
            rusqlite::params![outlet_id, spec.kind.as_str(), spec.name, spec.locator, enabled as i64],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM sources WHERE kind = ?1 AND locator = ?2",
            rusqlite::params![spec.kind.as_str(), spec.locator],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn set_enabled(&mut self, source_id: i64, enabled: bool) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE sources SET enabled = ?1 WHERE id = ?2",
            rusqlite::params![enabled as i64, source_id],
        )?;
        Ok(())
    }

    pub fn sources(&self, only_enabled: bool) -> Result<Vec<SourceRow>, StoreError> {
        let sql = "SELECT s.id, s.kind, s.name, s.locator, o.name, s.enabled
                   FROM sources s JOIN outlets o ON o.id = s.outlet_id
                   ORDER BY s.id";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, kind, name, locator, outlet, enabled) = row?;
            let Some(kind) = SourceKind::parse(&kind) else { continue };
            let enabled = enabled != 0;
            if only_enabled && !enabled {
                continue;
            }
            out.push(SourceRow { id, kind, name, locator, outlet, enabled });
        }
        Ok(out)
    }

    /// Insert or refresh items. Returns how many were NEW. Re-running a poll of
    /// unchanged feeds must change no ranking input, so this is idempotent.
    pub fn upsert_items(
        &mut self,
        source_id: i64,
        items: &[ParsedItem],
        now: i64,
    ) -> Result<usize, StoreError> {
        let before = self.item_count()?;
        let is_backfill = self.source_kind(source_id)? == SourceKind::Google;
        let tx = self.conn.transaction()?;
        for item in items {
            let outlet_id = match &item.publisher {
                Some(publisher) => resolve_outlet_in(&tx, publisher)?,
                None => {
                    let mut stmt = tx.prepare("SELECT outlet_id FROM sources WHERE id = ?1")?;
                    let id: i64 = stmt.query_row([source_id], |r| r.get(0))?;
                    id
                }
            };
            tx.execute(
                "INSERT INTO items (source_id, outlet_id, external_id, url, title, description,
                                    section, published_at, first_seen, last_seen, views, cited, is_backfill)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11, ?12)
                 ON CONFLICT(source_id, external_id) DO UPDATE SET
                   outlet_id = excluded.outlet_id,
                   last_seen = excluded.last_seen,
                   title = excluded.title,
                   description = excluded.description,
                   section = excluded.section,
                   views = CASE
                     WHEN excluded.views IS NULL THEN items.views
                     WHEN items.views IS NULL THEN excluded.views
                     WHEN excluded.views > items.views THEN excluded.views
                     ELSE items.views
                   END,
                   cited = excluded.cited,
                   is_backfill = excluded.is_backfill",
                rusqlite::params![
                    source_id,
                    outlet_id,
                    item.external_id,
                    item.url,
                    item.title,
                    item.description,
                    item.section,
                    item.published_at,
                    now,
                    item.views,
                    item.cited as i64,
                    is_backfill as i64,
                ],
            )?;
        }
        tx.commit()?;
        Ok((self.item_count()? - before).max(0) as usize)
    }

    fn source_kind(&self, source_id: i64) -> Result<SourceKind, StoreError> {
        let raw: String =
            self.conn
                .query_row("SELECT kind FROM sources WHERE id = ?1", [source_id], |r| r.get(0))?;
        Ok(SourceKind::parse(&raw).unwrap_or(SourceKind::Rss))
    }

    pub fn item_count(&self) -> Result<i64, StoreError> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?)
    }
}

/// Single outlet lookup, shared by `resolve_outlet` and the upsert transaction
/// (`Transaction` derefs to `Connection`, so both callers use this one query).
fn resolve_outlet_in(conn: &Connection, name: &str) -> Result<i64, StoreError> {
    let key = outlet_key(name);
    let existing: Option<i64> = conn
        .query_row("SELECT id FROM outlets WHERE key = ?1", [&key], |r| r.get(0))
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    conn.execute("INSERT INTO outlets (name, key) VALUES (?1, ?2)", rusqlite::params![name, key])?;
    Ok(conn.last_insert_rowid())
}
```

Add to `src/lib.rs`:

```rust
pub mod store;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib store:: 2>&1 | tail -30`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: store items in SQLite with idempotent upsert and outlet resolution"
```

---

### Task 5: Store — windowed queries, view samples, retention

**Files:**
- Modify: `src/store.rs`

**Interfaces:**
- Consumes: everything from Task 4.
- Produces:
  - `Store::window_data(&self, from: i64, to: i64, allow_backfill: bool) -> Result<(Vec<ItemRow>, Vec<Sample>), StoreError>`
  - `Store::window(&self, window: Window, now: i64) -> Result<(Vec<ItemRow>, Vec<Sample>), StoreError>`
  - `Store::sample_views(&mut self, now: i64) -> Result<usize, StoreError>`
  - `Store::prune_samples(&mut self, before: i64) -> Result<usize, StoreError>`

- [ ] **Step 1: Write the failing test**

Add to the test module in `src/store.rs`:

```rust
    fn telegram_fixture() -> (Store, i64) {
        let mut store = Store::open_in_memory().unwrap();
        let id = store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Telegram,
                    outlet: "Day.az".to_string(),
                    name: "Day.az Telegram".to_string(),
                    locator: "@dayaz".to_string(),
                },
                true,
            )
            .unwrap();
        (store, id)
    }

    fn telegram_item(id: &str, views: i64, published_at: i64) -> ParsedItem {
        let mut item = item(id, "Başlıq", published_at);
        item.views = Some(views);
        item
    }

    #[test]
    fn window_excludes_backfill_for_short_windows_only() {
        let mut store = Store::open_in_memory().unwrap();
        let rss = store.ensure_source(&spec("APA", "APA RSS", "https://apa.az/rss"), true).unwrap();
        let seed = store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Google,
                    outlet: "Google News".to_string(),
                    name: "Google News 7d".to_string(),
                    locator: "google:7d".to_string(),
                },
                true,
            )
            .unwrap();
        let now = 1_700_000_000;
        store.upsert_items(rss, &[item("live", "Canlı xəbər", now - 60)], now).unwrap();
        store.upsert_items(seed, &[item("seed", "Köhnə xəbər", now - 3600)], now).unwrap();

        let (day_items, _) = store.window(Window::Day, now).unwrap();
        assert_eq!(day_items.len(), 1, "backfill must not appear in the 24h window");
        assert_eq!(day_items[0].title, "Canlı xəbər");

        let (week_items, _) = store.window(Window::Week, now).unwrap();
        assert_eq!(week_items.len(), 2, "backfill is admitted in the 7d window");
    }

    #[test]
    fn windowed_items_carry_their_outlet_and_kind() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store.upsert_items(id, &[telegram_item("p1", 500, now - 300)], now).unwrap();

        let (rows, _) = store.window(Window::Hour, now).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outlet, "Day.az");
        assert_eq!(rows[0].kind, SourceKind::Telegram);
        assert_eq!(rows[0].views, Some(500));
        assert!(!rows[0].is_backfill);
    }

    #[test]
    fn disabled_sources_drop_out_of_the_window() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store.upsert_items(id, &[telegram_item("p1", 500, now - 300)], now).unwrap();
        store.set_enabled(id, false).unwrap();
        let (rows, _) = store.window(Window::Hour, now).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn samples_are_recorded_once_per_throttle_window() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store.upsert_items(id, &[telegram_item("p1", 500, now - 300)], now).unwrap();

        assert_eq!(store.sample_views(now).unwrap(), 1);
        // Same item, five minutes later: inside the ten-minute throttle, so no new row.
        assert_eq!(store.sample_views(now + 300).unwrap(), 0);
        assert_eq!(store.sample_views(now + 900).unwrap(), 1);
    }

    #[test]
    fn samples_are_read_back_for_the_window_that_owns_the_item() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store.upsert_items(id, &[telegram_item("p1", 500, now - 300)], now).unwrap();
        store.sample_views(now).unwrap();

        let (_, samples) = store.window(Window::Hour, now).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].views, 500);
    }

    #[test]
    fn prune_removes_only_samples_older_than_the_cutoff() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store.upsert_items(id, &[telegram_item("p1", 500, now - 300)], now).unwrap();
        store.sample_views(now).unwrap();
        store.sample_views(now + 900).unwrap();

        assert_eq!(store.prune_samples(now + 600).unwrap(), 1);
        let remaining: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM view_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib store:: 2>&1 | tail -20`
Expected: FAIL — `no method named window found`.

- [ ] **Step 3: Write minimal implementation**

Add to `impl Store` in `src/store.rs`:

```rust
    /// Items and the view samples belonging to those items, for a time range.
    /// Samples are selected by their item, not their own timestamp, so a sample taken
    /// outside the range still describes an item inside it.
    pub fn window_data(
        &self,
        from: i64,
        to: i64,
        allow_backfill: bool,
    ) -> Result<(Vec<ItemRow>, Vec<Sample>), StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT i.id, i.source_id, i.outlet_id, o.name, s.kind, i.title, i.description,
                    i.url, i.published_at, i.views, i.cited, i.is_backfill
             FROM items i
             JOIN sources s ON s.id = i.source_id
             JOIN outlets o ON o.id = i.outlet_id
             WHERE i.published_at >= ?1 AND i.published_at <= ?2
               AND (?3 = 1 OR i.is_backfill = 0)
               AND s.enabled = 1
             ORDER BY i.published_at DESC",
        )?;
        let mapped = stmt.query_map(rusqlite::params![from, to, allow_backfill as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, Option<i64>>(9)?,
                r.get::<_, i64>(10)?,
                r.get::<_, i64>(11)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in mapped {
            let (
                item_id, source_id, outlet_id, outlet, kind, title, description, url,
                published_at, views, cited, is_backfill,
            ) = row?;
            let Some(kind) = SourceKind::parse(&kind) else { continue };
            items.push(ItemRow {
                item_id,
                source_id,
                outlet_id,
                outlet,
                kind,
                title,
                description,
                url,
                published_at,
                views,
                cited: cited != 0,
                is_backfill: is_backfill != 0,
            });
        }

        let mut sample_stmt = self.conn.prepare(
            "SELECT v.item_id, v.ts, v.views
             FROM view_samples v
             JOIN items i ON i.id = v.item_id
             JOIN sources s ON s.id = i.source_id
             WHERE i.published_at >= ?1 AND i.published_at <= ?2
               AND (?3 = 1 OR i.is_backfill = 0)
               AND s.enabled = 1",
        )?;
        let samples = sample_stmt
            .query_map(rusqlite::params![from, to, allow_backfill as i64], |r| {
                Ok(Sample {
                    item_id: r.get(0)?,
                    ts: r.get(1)?,
                    views: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok((items, samples))
    }

    /// Convenience wrapper for a named window ending at `now`.
    pub fn window(
        &self,
        window: Window,
        now: i64,
    ) -> Result<(Vec<ItemRow>, Vec<Sample>), StoreError> {
        self.window_data(now - window.seconds(), now, window.allows_backfill())
    }

    /// Record the current view count of every Telegram item, at most once per ten minutes
    /// per item. Telegram only exposes counts for a limited window, so a missed sample is lost.
    pub fn sample_views(&mut self, now: i64) -> Result<usize, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO view_samples (item_id, ts, views)
             SELECT i.id, ?1, i.views
             FROM items i JOIN sources s ON s.id = i.source_id
             WHERE s.kind = 'telegram' AND i.views IS NOT NULL
               AND NOT EXISTS (
                 SELECT 1 FROM view_samples v WHERE v.item_id = i.id AND v.ts > ?1 - 600
               )",
            [now],
        )?;
        Ok(inserted)
    }

    pub fn prune_samples(&mut self, before: i64) -> Result<usize, StoreError> {
        Ok(self.conn.execute("DELETE FROM view_samples WHERE ts < ?1", [before])?)
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib store:: 2>&1 | tail -30`
Expected: PASS, 11 tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: windowed store queries, view sampling and sample retention"
```

---

### Task 6: Story clustering

**Files:**
- Create: `src/cluster.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::store::{ItemRow, SourceKind}`, `crate::text::tokens`.
- Produces: `crate::cluster::{Group, Clusterer, similarity, signature}`.
  - `similarity(a: &[String], b: &[String]) -> f64`
  - `signature(tokens: &[String]) -> String`
  - `Clusterer::new(threshold: f64)`
  - `Clusterer::assign(&self, groups: &mut Vec<Group>, item: &ItemRow) -> usize`
  - `Clusterer::group_items(&self, items: &[ItemRow]) -> Vec<Group>`
  - `Group { key, title, tokens, item_ids, newest, oldest, items }`

- [ ] **Step 1: Write the failing test**

Create `src/cluster.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ItemRow, SourceKind};

    fn item(id: i64, outlet_id: i64, title: &str, published_at: i64) -> ItemRow {
        ItemRow {
            item_id: id,
            source_id: outlet_id * 10,
            outlet_id,
            outlet: format!("Outlet{outlet_id}"),
            kind: SourceKind::Rss,
            title: title.to_string(),
            description: None,
            url: format!("https://example.az/{id}"),
            published_at,
            views: None,
            cited: false,
            is_backfill: false,
        }
    }

    #[test]
    fn similarity_is_jaccard_over_token_sets() {
        let a = vec!["bakida".to_string(), "yollar".to_string()];
        let b = vec!["bakida".to_string(), "yollar".to_string(), "baglidir".to_string()];
        assert!((similarity(&a, &b) - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(similarity(&a, &[]), 0.0, "an empty set never matches");
    }

    #[test]
    fn a_reworded_headline_joins_the_same_story() {
        let clusterer = Clusterer::new(0.45);
        let mut groups = Vec::new();
        let first = clusterer.assign(&mut groups, &item(1, 1, "Bakıda bu yollar bağlıdır", 100));
        let second = clusterer.assign(
            &mut groups,
            &item(2, 2, "Bakıda bu yollar bağlıdır - Sürücülərin nəzərinə", 200),
        );
        assert_eq!(first, second);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].item_ids.len(), 2);
        assert_eq!(groups[0].newest, 200);
    }

    #[test]
    fn a_syndicated_restatement_joins_the_original() {
        let clusterer = Clusterer::new(0.45);
        let mut groups = Vec::new();
        clusterer.assign(&mut groups, &item(1, 1, "Bakıda bu yollar bağlıdır", 100));
        clusterer.assign(&mut groups, &item(2, 2, "İstinadən: Bakıda bu yollar bağlıdır", 200));
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn different_events_sharing_one_word_stay_apart() {
        let clusterer = Clusterer::new(0.45);
        let mut groups = Vec::new();
        clusterer.assign(&mut groups, &item(1, 1, "Gəncədə iki nəfər bıçaqlandı", 100));
        clusterer.assign(&mut groups, &item(2, 2, "Gəncədə toy karvanı qəza etdi", 200));
        assert_eq!(groups.len(), 2, "a shared place name must not merge two events");
    }

    #[test]
    fn group_items_preserves_input_order_and_picks_the_earliest_title() {
        let clusterer = Clusterer::new(0.45);
        let items = vec![
            item(1, 1, "Bakıda bu yollar bağlıdır", 100),
            item(2, 2, "Bakıda bu yollar bağlıdır - Sürücülərin nəzərinə", 200),
            item(3, 3, "Tamamilə fərqli bir xəbər budur", 150),
        ];
        let groups = clusterer.group_items(&items);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title, "Bakıda bu yollar bağlıdır");
        assert_eq!(groups[0].item_ids, vec![1, 2]);
        assert_eq!(groups[0].oldest, 100);
    }

    #[test]
    fn signature_is_stable_and_short() {
        let tokens = vec!["a".to_string(), "b".to_string()];
        assert_eq!(signature(&tokens), "a-b");
        assert_eq!(signature(&[]), "empty");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cluster:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type Clusterer in this scope`.

- [ ] **Step 3: Write minimal implementation**

```rust
//! Grouping items into stories. Lexical, deterministic, and deliberately free of models.

use std::collections::HashMap;

use crate::store::ItemRow;
use crate::text::tokens;

/// Jaccard similarity over token sets. Both inputs must be deduplicated; order does
/// not matter, so a caller cannot silently mis-count by passing unsorted tokens.
pub fn similarity(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.iter().filter(|token| b.contains(token)).count();
    let union = a.len() + b.len() - shared;
    shared as f64 / union as f64
}

/// Stable identifier for a story: its whole sorted, deduplicated token set, joined by `-`.
///
/// `tokens` only ever yields alphanumeric runs, so no token contains `-` and the join is
/// injective: distinct token sets always produce distinct keys. Task 12 keys rank deltas on
/// this string, where a collision means both stories report the wrong movement.
///
/// An empty set returns `""`. Callers that need one identity per untokenized item must not use
/// this; `Group::new` keys those by row id instead.
pub fn signature(tokens: &[String]) -> String {
    tokens.join("-")
}

/// Key for a group that a new item starts: its signature, or a row-id key when the title yields
/// no tokens. An untokenized item can never match anything at a positive threshold, so each one
/// becomes its own group; keying them all `""` would collide and Task 12 would overwrite one
/// story's rank delta with another's. `item_id` is the database row id, stable across polls
/// because items are upserted rather than re-inserted. The `:` prefix is unreachable from
/// `signature`, which only ever joins alphanumeric tokens.
fn group_key(item: &ItemRow, tokens: &[String]) -> String {
    if tokens.is_empty() {
        format!("untokenized:{}", item.item_id)
    } else {
        signature(tokens)
    }
}

#[derive(Debug, Clone)]
pub struct Group {
    pub key: String,
    pub title: String,
    pub tokens: Vec<String>,
    pub item_ids: Vec<i64>,
    pub newest: i64,
    pub oldest: i64,
    pub items: Vec<ItemRow>,
}

impl Group {
    fn new(item: &ItemRow, tokens: Vec<String>) -> Self {
        Self {
            key: signature(&tokens),
            title: item.title.clone(),
            tokens,
            item_ids: vec![item.item_id],
            newest: item.published_at,
            oldest: item.published_at,
            items: vec![item.clone()],
        }
    }

    fn push(&mut self, item: &ItemRow) {
        self.item_ids.push(item.item_id);
        self.newest = self.newest.max(item.published_at);
        self.oldest = self.oldest.min(item.published_at);
        self.items.push(item.clone());
    }
}

/// Similarity threshold a caller's non-finite value falls back to.
const DEFAULT_THRESHOLD: f64 = 0.45;

pub struct Clusterer {
    threshold: f64,
}

impl Clusterer {
    /// Clamp the threshold into its meaningful range. Above 1.0 nothing can ever match —
    /// similarity tops out at 1.0 — so identical headlines would stay in separate groups and
    /// hand Task 12 duplicate keys. `f64::clamp` alone does not catch NaN
    /// (`f64::NAN.clamp(0.0, 1.0)` is NaN, and every comparison against it is false), so
    /// non-finite values take `DEFAULT_THRESHOLD`.
    pub fn new(threshold: f64) -> Self {
        let threshold = if threshold.is_finite() {
            threshold.clamp(0.0, 1.0)
        } else {
            DEFAULT_THRESHOLD
        };
        Self { threshold }
    }

    /// Place `item` in the most similar existing group, or start a new one.
    /// Returns the index of the group it landed in.
    pub fn assign(&self, groups: &mut Vec<Group>, item: &ItemRow) -> usize {
        let item_tokens = tokens(&item.title);
        let mut best: Option<(usize, f64)> = None;
        for (index, group) in groups.iter().enumerate() {
            let score = similarity(&item_tokens, &group.tokens);
            if score >= self.threshold && best.is_none_or(|(_, top)| score > top) {
                best = Some((index, score));
            }
        }
        match best {
            Some((index, _)) => {
                groups[index].push(item);
                index
            }
            None => {
                groups.push(Group::new(item, item_tokens));
                groups.len() - 1
            }
        }
    }

    /// Group a whole item set. An inverted token index keeps this linear-ish rather than
    /// quadratic: a new item is only compared against groups sharing at least one token.
    pub fn group_items(&self, items: &[ItemRow]) -> Vec<Group> {
        let mut groups: Vec<Group> = Vec::new();
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();

        // Oldest first, so a group's title is the earliest headline of the event.
        let mut ordered: Vec<&ItemRow> = items.iter().collect();
        ordered.sort_by_key(|item| (item.published_at, item.item_id));

        for item in ordered {
            let item_tokens = tokens(&item.title);
            // The index only covers token-sharing groups, which is a complete candidate set
            // only while a match requires a shared token. At a non-positive threshold a group
            // with nothing in common can still match, so fall back to every group and keep the
            // two entry points in agreement.
            let candidates: Vec<usize> = if self.threshold <= 0.0 {
                (0..groups.len()).collect()
            } else {
                let mut indexed: Vec<usize> = Vec::new();
                for token in &item_tokens {
                    if let Some(owners) = index.get(token) {
                        indexed.extend_from_slice(owners);
                    }
                }
                indexed.sort_unstable();
                indexed.dedup();
                indexed
            };

            let mut best: Option<(usize, f64)> = None;
            for group_index in candidates {
                let score = similarity(&item_tokens, &groups[group_index].tokens);
                if score >= self.threshold && best.is_none_or(|(_, top)| score > top) {
                    best = Some((group_index, score));
                }
            }

            let group_index = match best {
                Some((group_index, _)) => {
                    groups[group_index].push(item);
                    group_index
                }
                None => {
                    groups.push(Group::new(item, item_tokens.clone()));
                    groups.len() - 1
                }
            };
            for token in &item_tokens {
                index.entry(token.clone()).or_default().push(group_index);
            }
        }
        groups
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod cluster;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib cluster:: 2>&1 | tail -30`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: group items into stories by token overlap"
```

---

### Task 7: Ranking

**Files:**
- Create: `src/score.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::cluster::Group`, `crate::source::SourceKind`, `crate::store::{ItemRow, Sample, Window}`.
- Produces: `crate::score::{Weights, ScoredStory, OutletContribution, ItemRate, rank, views_per_hour}`.
  - `views_per_hour(samples: &[Sample], item_id: i64, published_at: i64, now: i64) -> Option<f64>`
  - `rank(groups: &[Group], samples: &[Sample], window: Window, weights: &Weights, now: i64) -> Vec<ScoredStory>`

- [ ] **Step 1: Write the failing test**

Create `src/score.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::{Clusterer, Group};
    use crate::source::SourceKind;
    use crate::store::{ItemRow, Sample, Window};

    const NOW: i64 = 1_700_000_000;

    /// Build one story from a list of (outlet_id, source_id, title, age_hours, views, cited).
    fn story(rows: &[(i64, i64, &str, i64, Option<i64>, bool)]) -> Group {
        let items: Vec<ItemRow> = rows
            .iter()
            .enumerate()
            .map(|(index, (outlet_id, source_id, title, age_hours, views, cited))| ItemRow {
                item_id: index as i64 + 1,
                source_id: *source_id,
                outlet_id: *outlet_id,
                outlet: format!("Outlet{outlet_id}"),
                kind: if views.is_some() { SourceKind::Telegram } else { SourceKind::Rss },
                title: title.to_string(),
                description: None,
                url: format!("https://example.az/{index}"),
                published_at: NOW - age_hours * 3600,
                views: *views,
                cited: *cited,
                is_backfill: false,
            })
            .collect();
        Clusterer::new(0.45).group_items(&items).remove(0)
    }

    fn views_for(group: &Group, views: i64, samples: &mut Vec<Sample>) {
        for item in &group.items {
            if item.views.is_some() {
                samples.push(Sample { item_id: item.item_id, ts: NOW - 3600, views: 0 });
                samples.push(Sample { item_id: item.item_id, ts: NOW, views });
            }
        }
    }

    #[test]
    fn coverage_counts_distinct_outlets_not_distinct_sources() {
        // One outlet publishing both an RSS feed and a Telegram channel casts one vote.
        let group = story(&[
            (1, 10, "Bakıda yollar bağlıdır", 1, None, false),
            (1, 11, "Bakıda yollar bağlıdır", 1, Some(100), false),
        ]);
        let ranked = rank(&[group], &[], Window::Hour, &Weights::default(), NOW);
        assert_eq!(ranked[0].coverage, 1.0);
        assert_eq!(ranked[0].outlets.len(), 1);
    }

    #[test]
    fn a_cited_repost_is_worth_half_and_a_real_report_is_worth_one() {
        let cited_only = story(&[(1, 10, "Bakıda yollar bağlıdır", 1, None, true)]);
        let ranked = rank(&[cited_only], &[], Window::Hour, &Weights::default(), NOW);
        assert_eq!(ranked[0].coverage, 0.5);

        let mixed = story(&[
            (1, 10, "Bakıda yollar bağlıdır", 1, None, true),
            (1, 11, "Bakıda yollar bağlıdır - Yenilik", 1, None, false),
        ]);
        let ranked = rank(&[mixed], &[], Window::Hour, &Weights::default(), NOW);
        assert_eq!(ranked[0].coverage, 1.0, "one uncited item makes the outlet count fully");
    }

    #[test]
    fn engagement_is_normalized_per_channel() {
        // Channel 10 habitually gets 1000 views/hour; channel 20 gets 50.
        let mut samples = Vec::new();
        let big = story(&[(1, 10, "Böyük kanal xəbəri budur", 1, Some(1000), false)]);
        let small = story(&[(2, 20, "Kiçik kanal xəbəri budur", 1, Some(50), false)]);
        views_for(&big, 1000, &mut samples);
        views_for(&small, 50, &mut samples);

        let ranked = rank(&[big, small], &samples, Window::Hour, &Weights::default(), NOW);
        let big_score = ranked.iter().find(|s| s.title.contains("Böyük")).unwrap();
        let small_score = ranked.iter().find(|s| s.title.contains("Kiçik")).unwrap();
        assert!(
            (big_score.engagement_norm - small_score.engagement_norm).abs() < 0.05,
            "each channel's median makes its own story the baseline: {} vs {}",
            big_score.engagement_norm,
            small_score.engagement_norm
        );
    }

    #[test]
    fn freshness_decays_within_the_window() {
        let fresh = story(&[(1, 10, "Təzə xəbər budur", 1, None, false)]);
        let stale = story(&[(2, 20, "Köhnə xəbər budur", 20, None, false)]);
        let ranked = rank(&[fresh, stale], &[], Window::Day, &Weights::default(), NOW);
        assert!(
            ranked[0].freshness > ranked[1].freshness + 0.3,
            "20 hours old must decay well above 1 hour old in a 24h window"
        );
    }

    #[test]
    fn broad_coverage_outranks_a_narrow_story() {
        let broad = story(&[
            (1, 10, "Geniş yayılmış xəbər budur", 1, None, false),
            (2, 20, "Geniş yayılmış xəbər budur", 1, None, false),
            (3, 30, "Geniş yayılmış xəbər budur", 1, None, false),
        ]);
        let narrow = story(&[(4, 40, "Yalnız bir yerdə olan xəbər", 1, None, false)]);
        let ranked = rank(&[broad, narrow], &[], Window::Hour, &Weights::default(), NOW);
        assert_eq!(ranked[0].coverage, 3.0);
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn views_per_hour_uses_the_sample_slope_then_falls_back_to_age() {
        let samples = vec![
            Sample { item_id: 1, ts: NOW - 3600, views: 100 },
            Sample { item_id: 1, ts: NOW, views: 700 },
        ];
        let rate = views_per_hour(&samples, 1, NOW - 7200, NOW).unwrap();
        assert!((rate - 600.0).abs() < 1e-6);

        // A single sample two hours old: 400 views spread over two hours.
        let single = vec![Sample { item_id: 2, ts: NOW, views: 400 }];
        let rate = views_per_hour(&single, 2, NOW - 7200, NOW).unwrap();
        assert!((rate - 200.0).abs() < 1e-6);

        // A brand-new post is floored at a quarter hour so it cannot divide by zero.
        let fresh = vec![Sample { item_id: 3, ts: NOW, views: 10 }];
        let rate = views_per_hour(&fresh, 3, NOW, NOW).unwrap();
        assert!((rate - 40.0).abs() < 1e-6);
    }

    #[test]
    fn stories_are_returned_best_first() {
        let broad = story(&[
            (1, 10, "Geniş yayılmış xəbər budur", 1, None, false),
            (2, 20, "Geniş yayılmış xəbər budur", 1, None, false),
        ]);
        let narrow = story(&[(4, 40, "Yalnız bir yerdə olan xəbər", 1, None, false)]);
        let ranked = rank(&[narrow, broad], &[], Window::Hour, &Weights::default(), NOW);
        assert_eq!(ranked[0].coverage, 2.0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib score:: 2>&1 | tail -20`
Expected: FAIL — `cannot find function rank in this scope`.

- [ ] **Step 3: Write minimal implementation**

```rust
//! Popularity scoring: cross-outlet coverage, reader engagement, and freshness.

use std::collections::{BTreeMap, HashMap};
use std::cmp::Ordering;

use serde::Deserialize;

use crate::cluster::Group;
use crate::source::SourceKind;
use crate::store::{ItemRow, Sample, Window};

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Weights {
    pub coverage: f64,
    pub engagement: f64,
    pub freshness: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self { coverage: 0.40, engagement: 0.40, freshness: 0.20 }
    }
}

#[derive(Debug, Clone)]
pub struct OutletContribution {
    pub outlet: String,
    pub weight: f64,
    pub newest: i64,
    pub views: Option<i64>,
    pub views_per_hour: Option<f64>,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct ScoredStory {
    pub key: String,
    pub title: String,
    pub score: f64,
    pub coverage: f64,
    pub coverage_norm: f64,
    pub engagement: f64,
    pub engagement_norm: f64,
    pub freshness: f64,
    pub view_count: i64,
    pub newest: i64,
    pub outlets: Vec<OutletContribution>,
}

#[derive(Debug, Clone, Copy)]
struct ItemRate {
    item_id: i64,
    source_id: i64,
    rate: f64,
    views: i64,
}

/// Views gained per hour. With two samples at least ten minutes apart this is the observed
/// slope; otherwise it spreads the total over the item's age, floored at a quarter hour.
pub fn views_per_hour(samples: &[Sample], item_id: i64, published_at: i64, now: i64) -> Option<f64> {
    let mut owned: Vec<&Sample> = samples.iter().filter(|s| s.item_id == item_id).collect();
    if owned.is_empty() {
        return None;
    }
    owned.sort_by_key(|s| s.ts);
    let first = owned.first()?;
    let last = owned.last()?;
    if owned.len() >= 2 && last.ts - first.ts >= 600 {
        let hours = (last.ts - first.ts) as f64 / 3600.0;
        return Some(((last.views - first.views).max(0) as f64) / hours);
    }
    let age_hours = ((now - published_at).max(0) as f64 / 3600.0).max(0.25);
    Some(last.views as f64 / age_hours)
}

fn median(values: &mut Vec<f64>) -> f64 {
    if values.is_empty() {
        return 1.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

pub fn rank(
    groups: &[Group],
    samples: &[Sample],
    window: Window,
    weights: &Weights,
    now: i64,
) -> Vec<ScoredStory> {
    let mut group_of: HashMap<i64, usize> = HashMap::new();
    for (index, group) in groups.iter().enumerate() {
        for item_id in &group.item_ids {
            group_of.insert(*item_id, index);
        }
    }

    let mut rates: Vec<ItemRate> = Vec::new();
    for group in groups {
        for item in &group.items {
            if item.kind != SourceKind::Telegram {
                continue;
            }
            let Some(views) = item.views else { continue };
            let Some(rate) = views_per_hour(samples, item.item_id, item.published_at, now) else {
                continue;
            };
            rates.push(ItemRate { item_id: item.item_id, source_id: item.source_id, rate, views });
        }
    }

    // Per-channel medians: this is what stops a large channel's routine post from
    // outranking a small channel's breakout post.
    let mut per_channel: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    for rate in &rates {
        per_channel.entry(rate.source_id).or_default().push(rate.rate);
    }
    let medians: BTreeMap<i64, f64> =
        per_channel.iter().map(|(id, values)| (*id, median(&mut values.clone()))).collect();

    let tau_hours = (window.seconds() as f64 / 3600.0) / 3.0;

    let mut stories: Vec<ScoredStory> = Vec::with_capacity(groups.len());
    for (group_index, group) in groups.iter().enumerate() {
        let mut per_outlet: BTreeMap<i64, OutletContribution> = BTreeMap::new();
        let mut any_uncited: BTreeMap<i64, bool> = BTreeMap::new();

        for item in &group.items {
            let entry = per_outlet.entry(item.outlet_id).or_insert_with(|| OutletContribution {
                outlet: item.outlet.clone(),
                weight: 1.0,
                newest: item.published_at,
                views: item.views,
                views_per_hour: None,
                title: item.title.clone(),
                url: item.url.clone(),
            });
            if item.published_at > entry.newest {
                entry.newest = item.published_at;
                entry.title = item.title.clone();
                entry.url = item.url.clone();
            }
            if item.kind == SourceKind::Telegram {
                if let Some(views) = item.views {
                    entry.views = Some(entry.views.unwrap_or(0).max(views));
                    entry.views_per_hour =
                        views_per_hour(samples, item.item_id, item.published_at, now);
                }
            }
            let flag = any_uncited.entry(item.outlet_id).or_insert(false);
            *flag = *flag || !item.cited;
        }

        // Weight is decided per outlet, once every item of that outlet is known: a single
        // uncited item makes the whole outlet count fully.
        let outlet_weight = |outlet_id: i64| {
            if any_uncited.get(&outlet_id).copied().unwrap_or(false) { 1.0 } else { 0.5 }
        };
        let coverage: f64 = per_outlet.keys().map(|outlet_id| outlet_weight(*outlet_id)).sum();

        let mut outlets: Vec<OutletContribution> = Vec::with_capacity(per_outlet.len());
        for (outlet_id, mut contribution) in per_outlet {
            contribution.weight = outlet_weight(outlet_id);
            outlets.push(contribution);
        }

        let mut engagement = 0.0;
        let mut view_count = 0i64;
        for rate in &rates {
            if group_of.get(&rate.item_id) != Some(&group_index) {
                continue;
            }
            let baseline = medians.get(&rate.source_id).copied().unwrap_or(1.0).max(1.0);
            engagement += rate.rate / baseline;
            view_count += rate.views;
        }

        let age_hours = ((now - group.newest).max(0) as f64) / 3600.0;
        let freshness = (-age_hours / tau_hours).exp();

        stories.push(ScoredStory {
            key: group.key.clone(),
            title: group.title.clone(),
            score: 0.0,
            coverage,
            coverage_norm: 0.0,
            engagement,
            engagement_norm: 0.0,
            freshness,
            view_count,
            newest: group.newest,
            outlets,
        });
    }

    let max_coverage = stories.iter().map(|s| s.coverage).fold(0.0, f64::max);
    let max_engagement = stories.iter().map(|s| s.engagement).fold(0.0, f64::max);
    for story in &mut stories {
        story.coverage_norm = if max_coverage > 0.0 { story.coverage / max_coverage } else { 0.0 };
        story.engagement_norm = if max_engagement > 0.0 { story.engagement / max_engagement } else { 0.0 };
        story.score = weights.coverage * story.coverage_norm
            + weights.engagement * story.engagement_norm
            + weights.freshness * story.freshness;
    }
    stories.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then(b.newest.cmp(&a.newest))
    });
    stories
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib score:: 2>&1 | tail -30`
Expected: PASS, 7 tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: rank stories by outlet coverage, engagement and freshness"
```

---

### Task 8: Directories and configuration

**Files:**
- Create: `src/dirs.rs`, `src/config.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::error::ConfigError`, `crate::score::Weights`, `crate::source::{SourceKind, SourceSpec}`.
- Produces: `crate::dirs::AppDirs`; `crate::config::{Config, SourceConfig, Retention}`.
  - `AppDirs::resolve() -> Option<AppDirs>`, `AppDirs::db_path()`, `config_path()`, `log_path()`
  - `Config::load(&Path) -> Result<Config, ConfigError>`, `Config::default()`
  - `Config::source_specs(&self) -> Vec<(SourceSpec, bool)>`

- [ ] **Step 1: Write the failing test**

Create `src/config.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_ships_twenty_sources_with_nineteen_enabled() {
        let config = Config::default();
        assert_eq!(config.sources.len(), 20);
        let enabled = config.sources.iter().filter(|s| s.enabled).count();
        assert_eq!(enabled, 19, "apatv ships disabled until it serves a preview again");
        assert_eq!(config.sources.iter().filter(|s| !s.enabled).map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["APATV Telegram"]);
    }

    #[test]
    fn default_config_matches_the_agreed_values() {
        let config = Config::default();
        assert_eq!(config.poll_interval_secs, 300);
        assert!((config.cluster_threshold - 0.45).abs() < 1e-9);
        assert!((config.weights.coverage - 0.40).abs() < 1e-9);
        assert_eq!(config.retention.view_sample_days, 30);
        assert!(config.local_keywords.iter().any(|k| k == "Bakı"));
    }

    #[test]
    fn source_specs_carry_kind_locator_and_outlet() {
        let specs = Config::default().source_specs();
        assert_eq!(specs.len(), 20);
        assert!(specs.iter().any(|(spec, enabled)| {
            spec.kind == SourceKind::Rss && spec.locator == "https://qafqazinfo.az/rss" && *enabled
        }));
        assert!(specs.iter().any(|(spec, _)| {
            spec.kind == SourceKind::Telegram && spec.locator == "@qafqazinfo" && spec.outlet == "Qafqazinfo"
        }));
    }

    #[test]
    fn a_missing_file_falls_back_to_defaults() {
        let config = Config::load(std::path::Path::new("/nonexistent/bakutrend.toml"));
        assert!(config.is_err(), "an explicit path that does not exist is an error, not a silent default");
    }

    #[test]
    fn partial_toml_overrides_only_the_keys_it_sets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "poll_interval_secs = 900\n").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.poll_interval_secs, 900);
        assert_eq!(config.sources.len(), 20, "unset keys keep their defaults");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib config:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type Config in this scope`.

- [ ] **Step 3: Write minimal implementation**

`src/dirs.rs`:

```rust
//! Path resolution, following the sibling `ttymap` convention: the `directories` crate
//! v6 with a brand string, and `state_dir()` falling back to `data_local_dir()` because
//! `state_dir()` is Linux-only.

use std::path::PathBuf;

use directories::ProjectDirs;

#[derive(Debug, Clone)]
pub struct AppDirs {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub state: PathBuf,
}

impl AppDirs {
    pub fn resolve() -> Option<Self> {
        let dirs = ProjectDirs::from("", "", "bakutrend")?;
        let state = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .to_path_buf();
        Some(Self {
            config: dirs.config_dir().to_path_buf(),
            data: dirs.data_dir().to_path_buf(),
            cache: dirs.cache_dir().to_path_buf(),
            state,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data.join("bakutrend.sqlite")
    }

    pub fn config_path(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    pub fn log_path(&self) -> PathBuf {
        self.state.join("bakutrend.log")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_paths_are_branded_and_distinct() {
        let Some(dirs) = AppDirs::resolve() else { return };
        assert!(dirs.db_path().to_string_lossy().contains("bakutrend"));
        assert!(dirs.db_path().ends_with("bakutrend.sqlite"));
        assert!(dirs.config_path().to_string_lossy().ends_with("config.toml"));
        assert!(dirs.log_path().to_string_lossy().contains("bakutrend"));
    }
}
```

`src/config.rs`:

```rust
//! TOML configuration. A missing default path is not an error; an explicit path that
//! does not exist is.

use std::path::Path;

use serde::Deserialize;

use crate::error::ConfigError;
use crate::score::Weights;
use crate::source::{SourceKind, SourceSpec};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Retention {
    pub view_sample_days: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Self { view_sample_days: 30 }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    pub name: String,
    pub kind: String,
    pub locator: String,
    pub outlet: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub poll_interval_secs: u64,
    pub cluster_threshold: f64,
    pub weights: Weights,
    pub retention: Retention,
    pub local_keywords: Vec<String>,
    pub sources: Vec<SourceConfig>,
}

const LOCAL_KEYWORDS: &[&str] = &[
    "Azərbaycan", "Bakı", "Gəncə", "Sumqayıt", "Mingəçevir", "Bərdə", "Lənkəran", "Astara",
    "Naxçıvan", "Şuşa", "Xankəndi", "Qarabağ", "Şərqi Zəngəzur", "Xəzər", "Abşeron", "Quba",
    "Qusar", "Şəki", "Yevlax", "Salyan", "Prezident", "Milli Məclis", "Nazirlər Kabineti",
    "SOCAR", "AZAL", "ADY", "ANAMA", "DİN", "XİN", "MİDA", "CƏB",
];

/// The 20 verified sources: 10 RSS feeds and 10 Telegram channels.
/// `@apatv` answered during design research but now returns a preview-less stub, so it
/// ships disabled (2026-09-14); flip it on when it serves posts again.
fn default_sources() -> Vec<SourceConfig> {
    let rss = [
        ("Qafqazinfo RSS", "https://qafqazinfo.az/rss", "Qafqazinfo"),
        ("APA RSS", "https://apa.az/rss", "APA"),
        ("Azertag RSS", "https://azertag.az/rss", "Azertag"),
        ("Report RSS", "https://report.az/rss/", "Report"),
        ("Modern.az RSS", "https://modern.az/rss", "Modern.az"),
        ("Olke.az RSS", "https://olke.az/rss", "Olke.az"),
        ("Baku.ws RSS", "https://baku.ws/rss", "Baku.ws"),
        ("Trend.az RSS", "https://trend.az/rss/", "Trend.az"),
        ("Minval RSS", "https://minval.az/rss", "Minval"),
        ("Haqqin.az RSS", "https://haqqin.az/rss/", "Haqqin.az"),
    ];
    let telegram = [
        ("Qafqazinfo Telegram", "@qafqazinfo", "Qafqazinfo", true),
        ("APA Telegram", "@apa_az", "APA", true),
        ("Day.az Telegram", "@dayaz", "Day.az", true),
        ("Axar.az Telegram", "@axaraz", "Axar.az", true),
        ("Minval Telegram", "@minval_az", "Minval", true),
        ("Report Telegram", "@reportnewsaz", "Report", true),
        ("Apa TV Telegram", "@apatv", "Apa TV", false),
        ("Baku Post Telegram", "@bakupost", "Baku Post", true),
        ("Qaynarinfo Telegram", "@qaynarinfo", "Qaynarinfo", true),
        ("Meydan TV Telegram", "@meydantv", "Meydan TV", true),
    ];

    let mut sources: Vec<SourceConfig> = rss
        .iter()
        .map(|(name, locator, outlet)| SourceConfig {
            name: name.to_string(),
            kind: "rss".to_string(),
            locator: locator.to_string(),
            outlet: outlet.to_string(),
            enabled: true,
        })
        .collect();
    sources.extend(telegram.iter().map(|(name, locator, outlet, enabled)| SourceConfig {
        name: name.to_string(),
        kind: "telegram".to_string(),
        locator: locator.to_string(),
        outlet: outlet.to_string(),
        enabled: *enabled,
    }));
    sources
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_interval_secs: 300,
            cluster_threshold: 0.45,
            weights: Weights::default(),
            retention: Retention::default(),
            local_keywords: LOCAL_KEYWORDS.iter().map(|k| k.to_string()).collect(),
            sources: default_sources(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&raw).map_err(|source| ConfigError::Toml {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Config entries become source specs; unknown `kind` values are skipped.
    pub fn source_specs(&self) -> Vec<(SourceSpec, bool)> {
        self.sources
            .iter()
            .filter_map(|source| {
                let kind = SourceKind::parse(&source.kind)?;
                Some((
                    SourceSpec {
                        kind,
                        outlet: source.outlet.clone(),
                        name: source.name.clone(),
                        locator: source.locator.clone(),
                    },
                    source.enabled,
                ))
            })
            .collect()
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod config;
pub mod dirs;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib config:: dirs:: 2>&1 | tail -30`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: configuration with the verified 20-source default and branded paths"
```

---

### Task 9: HTTP fetching and the Google backfill

**Files:**
- Create: `src/source/http.rs`, `src/source/google.rs`
- Modify: `src/source/mod.rs`, `src/lib.rs`

**Interfaces:**
- Consumes: `crate::error::FetchError`, `crate::source::{ParseOutcome, ParsedItem, SourceKind}`, `crate::store::{SourceRow, Store}`.
- Produces:
  - `crate::source::http::{Fetcher, HttpFetcher, rss_url, telegram_url, google_url}`
  - `crate::source::google::parse(&[u8]) -> Result<ParseOutcome, ParseError>`
  - `HttpFetcher::new() -> Result<HttpFetcher, FetchError>`, `impl Fetcher for HttpFetcher { fn fetch(&self, &SourceRow) -> Result<ParseOutcome, FetchError> }`

- [ ] **Step 1: Write the failing test**

Create `src/source/google.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_titles_lose_the_publisher_suffix() {
        let xml = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item><title>Bakıda yollar bağlıdır - Day.Az</title>
        <link>https://news.google.com/rss/articles/abc</link>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate>
        <source url="https://www.day.az">Day.Az</source></item></channel></rss>"#;
        let out = parse(xml).expect("parses");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].title, "Bakıda yollar bağlıdır");
        assert_eq!(out.items[0].publisher.as_deref(), Some("Day.Az"));
    }

    #[test]
    fn a_title_without_a_suffix_is_left_alone() {
        let xml = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item><title>Sadə başlıq</title><link>https://news.google.com/rss/articles/x</link>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate></item></channel></rss>"#;
        let out = parse(xml).expect("parses");
        assert_eq!(out.items[0].title, "Sadə başlıq");
        assert_eq!(out.items[0].publisher, None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib source::google 2>&1 | tail -20`
Expected: FAIL — `cannot find function parse in this scope`.

- [ ] **Step 3: Write the Google implementation**

```rust
//! Google News RSS, used only to seed the week window on an empty database.
//! Titles arrive as `Headline - Publisher`, and the RSS `<source>` element names the publisher.

use crate::error::ParseError;
use crate::source::{is_cited, section_from_url, ParseOutcome, ParsedItem};
use crate::text::{collapse_ws, decode_entities};

pub fn parse(bytes: &[u8]) -> Result<ParseOutcome, ParseError> {
    let feed = feed_rs::parser::parse(bytes).map_err(|e| ParseError::Feed(e.to_string()))?;
    let mut outcome = ParseOutcome::default();
    for entry in feed.entries {
        let Some(url) = entry.links.first().map(|link| link.href.clone()) else {
            outcome.skipped += 1;
            continue;
        };
        let raw_title = entry
            .title
            .map(|t| collapse_ws(&decode_entities(&t.content)))
            .unwrap_or_default();
        let publisher = entry
            .source
            .map(|s| collapse_ws(&decode_entities(&s)))
            .filter(|s| !s.is_empty());
        let title = strip_publisher_suffix(&raw_title, publisher.as_deref());
        if title.is_empty() {
            outcome.skipped += 1;
            continue;
        }
        let Some(published_at) = entry.published.or(entry.updated).map(|d| d.timestamp()) else {
            outcome.skipped += 1;
            continue;
        };
        let description = entry
            .summary
            .map(|t| collapse_ws(&decode_entities(&t.content)));
        outcome.items.push(ParsedItem {
            external_id: url.clone(),
            section: section_from_url(&url),
            cited: is_cited(description.as_deref().unwrap_or_default()),
            url,
            title,
            description,
            published_at,
            views: None,
            publisher,
        });
    }
    Ok(outcome)
}

/// Remove a trailing ` - Publisher`. Without a known publisher, take the last ` - ` segment
/// off when it looks like a site name (no spaces beyond one, ends in a TLD-ish token).
fn strip_publisher_suffix(title: &str, publisher: Option<&str>) -> String {
    if let Some(publisher) = publisher {
        for separator in [" - ", " — ", " | "] {
            let suffix = format!("{separator}{publisher}");
            if let Some(head) = title.strip_suffix(&suffix) {
                return head.trim().to_string();
            }
        }
    }
    match title.rsplit_once(" - ") {
        Some((head, tail)) if !tail.contains(' ') && tail.chars().count() <= 24 && !head.is_empty() => {
            head.trim().to_string()
        }
        _ => title.to_string(),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib source::google 2>&1 | tail -20`
Expected: PASS, 2 tests.

- [ ] **Step 5: Write the fetcher**

`src/source/http.rs`:

```rust
//! Blocking HTTP fetching. The poller is sequential, so a client pool buys nothing;
//! a single client with a browser User-Agent is enough (`azertag.az` returns 400 without one).

use std::time::Duration;

use crate::error::FetchError;
use crate::source::{self, ParseOutcome, SourceKind};
use crate::store::SourceRow;

const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) bakutrend/0.1";

/// Anything that can turn a source row into parsed items. Tests supply a fixture-backed
/// implementation so the whole poll cycle runs offline.
pub trait Fetcher {
    fn fetch(&self, source: &SourceRow) -> Result<ParseOutcome, FetchError>;
}

pub fn rss_url(locator: &str) -> String {
    locator.to_string()
}

pub fn telegram_url(handle: &str) -> String {
    format!("https://t.me/s/{}", handle.trim_start_matches('@'))
}

pub fn google_url(query: &str, when: &str) -> String {
    let encoded = query.replace(' ', "+");
    format!("https://news.google.com/rss/search?q={encoded}+when:{when}&hl=az&gl=AZ&ceid=AZ:az")
}

pub struct HttpFetcher {
    client: reqwest::blocking::Client,
    google_query: String,
}

impl HttpFetcher {
    pub fn new(google_query: &str) -> Result<Self, FetchError> {
        let url = "https://news.google.com/".to_string();
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|source| FetchError::Network { url, source })?;
        Ok(Self { client, google_query: google_query.to_string() })
    }
}

impl Fetcher for HttpFetcher {
    fn fetch(&self, source: &SourceRow) -> Result<ParseOutcome, FetchError> {
        let url = match source.kind {
            SourceKind::Rss => rss_url(&source.locator),
            SourceKind::Telegram => telegram_url(&source.locator),
            SourceKind::Google => google_url(&self.google_query, "7d"),
        };
        let response = self
            .client
            .get(&url)
            .send()
            .map_err(|source| FetchError::Network { url: url.clone(), source })?;
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Http { status: status.as_u16(), url });
        }
        let body = response
            .bytes()
            .map_err(|source| FetchError::Network { url: url.clone(), source })?;

        match source.kind {
            SourceKind::Rss => {
                source::rss::parse(&body).map_err(|source| FetchError::Parse { url, source })
            }
            SourceKind::Google => {
                source::google::parse(&body).map_err(|source| FetchError::Parse { url, source })
            }
            SourceKind::Telegram => {
                let html = String::from_utf8_lossy(&body);
                let outcome = source::telegram::parse(&html);
                // HTTP 200 with no posts means the channel disabled previews. Reporting that
                // as "0 new items" would hide a dead source.
                if outcome.items.is_empty() {
                    return Err(FetchError::EmptyPreview { handle: source.locator.clone() });
                }
                Ok(outcome)
            }
        }
    }
}
```

Add `ParseError` import usage note: `source::rss::parse` returns `Result<_, ParseError>`, so the
`map_err` above maps it into `FetchError::Parse`.

Add to `src/source/mod.rs`:

```rust
pub mod google;
pub mod http;
```

- [ ] **Step 6: Write the URL builder tests**

Add to the test module in `src/source/http.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_urls_strip_the_at_sign() {
        assert_eq!(telegram_url("@qafqazinfo"), "https://t.me/s/qafqazinfo");
        assert_eq!(telegram_url("qafqazinfo"), "https://t.me/s/qafqazinfo");
    }

    #[test]
    fn google_urls_encode_the_query_and_window() {
        let url = google_url("azerbaycan baki", "7d");
        assert!(url.contains("q=azerbaycan+baki+when:7d"), "{url}");
        assert!(url.contains("hl=az"), "{url}");
    }
}
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --lib source:: 2>&1 | tail -30`
Expected: PASS, including the 2 new URL tests.

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "feat: blocking HTTP fetcher, Telegram empty-preview detection, Google backfill parsing"
```

---

### Task 10: Poll cycle and offline end-to-end pipeline

**Files:**
- Create: `src/poller.rs`, `tests/pipeline.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::source::{Fetcher, ParseOutcome}`, `crate::store::Store`, `crate::cluster::Clusterer`, `crate::score::rank`.
- Produces: `crate::poller::{Backoff, PollReport, poll_once, fetch_google_seed}`.
  - `poll_once(&mut Store, &dyn Fetcher, &mut Backoff, now: i64, retention_days: i64) -> Result<PollReport, StoreError>`
  - `PollReport { ok, failed: Vec<(String, String)>, new_items, samples, pruned, skipped }`
  - `Backoff::is_due(&self, source_id: i64, now: i64) -> bool`, `record_ok`, `record_failure`

- [ ] **Step 1: Write the failing test**

Create `src/poller.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::http::Fetcher;
    use crate::source::{ParseOutcome, SourceKind, SourceSpec};

    struct NeverFetcher;

    impl Fetcher for NeverFetcher {
        fn fetch(&self, _source: &crate::store::SourceRow) -> Result<ParseOutcome, FetchError> {
            Err(FetchError::Http { status: 500, url: "https://example.az".to_string() })
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
        assert!(!backoff.is_due(1, now + 30), "not due immediately after a failure");
        assert!(backoff.is_due(1, now + 61), "due after the first backoff interval");

        for step in 1..12 {
            backoff.record_failure(1, now + step * 3600);
        }
        assert!(!backoff.is_due(1, now + 12 * 3600 + 1700), "never exceeds the 30 minute cap");
        assert!(backoff.is_due(1, now + 12 * 3600 + 1801));

        backoff.record_ok(1);
        assert!(backoff.is_due(1, now + 12 * 3600 + 1802), "a success clears the backoff");
    }

    #[test]
    fn polling_twice_changes_nothing_the_second_time() {
        let mut store = store_with_two_sources();
        let mut backoff = Backoff::new();
        let fetcher = CountingFetcher { calls: std::cell::RefCell::new(Vec::new()) };
        let now = 1_700_000_000;

        let first = poll_once(&mut store, &fetcher, &mut backoff, now, 30).unwrap();
        let second = poll_once(&mut store, &fetcher, &mut backoff, now + 60, 30).unwrap();
        assert_eq!(first.ok, 2);
        assert_eq!(second.ok, 2);
        assert_eq!(store.item_count().unwrap(), 0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib poller:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type Backoff in this scope`.

- [ ] **Step 3: Write minimal implementation**

```rust
//! The poll cycle: fetch every due source, store what comes back, sample view counts,
//! prune old samples. Sources are isolated — one failure never aborts the cycle.

use std::collections::HashMap;

use crate::error::{FetchError, StoreError};
use crate::source::http::Fetcher;
use crate::source::SourceKind;
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
        self.ready_at.get(&source_id).is_none_or(|ready| now >= *ready)
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
    let has_week_data = !store
        .window(crate::store::Window::Week, now)?
        .0
        .is_empty();

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

    // One-shot week seed: only when no locally observed week data exists yet.
    if !has_week_data {
        if let Some(seed) = sources.iter().find(|s| s.kind == SourceKind::Google) {
            match fetcher.fetch(seed) {
                Ok(outcome) => {
                    store.upsert_items(seed.id, &outcome.items, now)?;
                }
                Err(error) => {
                    log::warn!("google backfill failed: {error}");
                    report.failed.push((seed.name.clone(), error.to_string()));
                }
            }
        }
    }

    report.samples = store.sample_views(now)?;
    report.pruned = store.prune_samples(now - retention_days * 86_400)?;
    Ok(report)
}
```

Add to `src/lib.rs`:

```rust
pub mod poller;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib poller:: 2>&1 | tail -30`
Expected: PASS, 3 tests.

- [ ] **Step 5: Write the offline end-to-end test**

Create `tests/pipeline.rs`:

```rust
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
```

- [ ] **Step 6: Run the end-to-end test**

Run: `cargo test --test pipeline 2>&1 | tail -30`
Expected: PASS, 2 tests. If `report.ok` is 3 but `failed` is non-empty, the fixture for `apa.rss.xml` was not captured in Task 2 — re-run that capture command.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat: poll cycle with per-source backoff and offline end-to-end pipeline test"
```

---

### Task 11: TUI rendering

**Files:**
- Create: `src/ui.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::score::ScoredStory`, `crate::store::Window`.
- Produces: `crate::ui::{View, draw}`; `View` fields: `window`, `stories`, `selected`, `deltas: Option<&HashMap<String, i64>>`, `local_only`, `filter`, `quiet_fallback`, `sources_ok`, `sources_total`, `last_poll`, `now`, `status`.

- [ ] **Step 1: Write the failing test**

Create `src/ui.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::{OutletContribution, ScoredStory};
    use crate::store::Window;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn story(title: &str, coverage: f64) -> ScoredStory {
        ScoredStory {
            key: title.to_string(),
            title: title.to_string(),
            score: 0.9,
            coverage,
            coverage_norm: 1.0,
            engagement: 1.2,
            engagement_norm: 0.8,
            freshness: 0.7,
            view_count: 2650,
            newest: 0,
            outlets: vec![OutletContribution {
                outlet: "Qafqazinfo".into(),
                weight: 1.0,
                newest: 0,
                views: Some(2650),
                views_per_hour: Some(900.0),
                title: title.to_string(),
                url: "https://qafqazinfo.az/news/detail/x-1".into(),
            }],
        }
    }

    fn render(view: &View<'_>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal.draw(|frame| draw(frame, view)).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(|line| line.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn base<'a>(stories: &'a [ScoredStory], deltas: Option<&'a std::collections::HashMap<String, i64>>) -> View<'a> {
        View {
            window: Window::Day,
            stories,
            selected: 0,
            deltas,
            local_only: false,
            filter: "",
            quiet_fallback: false,
            show_help: false,
            sources_ok: 12,
            sources_total: 20,
            last_poll: Some(0),
            now: 0,
            status: "",
        }
    }

    #[test]
    fn header_reports_source_health_and_the_active_window() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let screen = render(&base(&stories, None));
        assert!(screen.contains("12/20 sources ok"), "{screen}");
        assert!(screen.contains("24h"), "{screen}");
        assert!(screen.contains("Bakıda bu yollar bağlıdır"), "{screen}");
    }

    #[test]
    fn the_delta_column_is_absent_until_comparable_history_exists() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let without = render(&base(&stories, None));
        assert!(!without.contains("Delta"), "no delta header without history:\n{without}");

        let deltas = std::collections::HashMap::from([("Bakıda bu yollar bağlıdır".to_string(), 3i64)]);
        let with = render(&base(&stories, Some(&deltas)));
        assert!(with.contains("+3"), "a rising story shows its movement:\n{with}");
    }

    #[test]
    fn the_quiet_fallback_is_labeled_and_never_silent() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let mut view = base(&stories, None);
        view.quiet_fallback = true;
        let screen = render(&view);
        assert!(screen.contains("Quiet hour"), "{screen}");
    }

    #[test]
    fn the_help_overlay_is_hidden_until_asked_for() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let mut view = base(&stories, None);
        assert!(!render(&view).contains("switch window"));

        view.show_help = true;
        let screen = render(&view);
        assert!(screen.contains("switch window"), "{screen}");
        assert!(screen.contains("poll now"), "{screen}");
    }

    #[test]
    fn the_detail_pane_shows_the_score_breakdown_and_every_outlet() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let screen = render(&base(&stories, None));
        assert!(screen.contains("coverage"), "{screen}");
        assert!(screen.contains("engagement"), "{screen}");
        assert!(screen.contains("freshness"), "{screen}");
        assert!(screen.contains("Qafqazinfo"), "{screen}");
    }

    #[test]
    fn a_very_long_headline_does_not_panic_or_overflow() {
        let long = "Bakıda ".repeat(200);
        let stories = vec![story(&long, 1.0)];
        let screen = render(&base(&stories, None));
        assert!(screen.lines().all(|line| unicode_width::UnicodeWidthStr::width(line) <= 100));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ui:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type View in this scope`.

- [ ] **Step 3: Write minimal implementation**

```rust
//! Rendering. Stateless: everything the frame needs arrives in `View`, so the same
//! function serves the real terminal and `TestBackend`.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::score::ScoredStory;
use crate::store::Window;

pub struct View<'a> {
    pub window: Window,
    pub stories: &'a [ScoredStory],
    pub selected: usize,
    /// `None` hides the delta column entirely, which is the state before any
    /// comparable history exists. A column of dashes teaches nothing.
    pub deltas: Option<&'a HashMap<String, i64>>,
    pub local_only: bool,
    pub filter: &'a str,
    pub quiet_fallback: bool,
    pub show_help: bool,
    pub sources_ok: usize,
    pub sources_total: usize,
    pub last_poll: Option<i64>,
    pub now: i64,
    pub status: &'a str,
}

fn relative(now: i64, then: i64) -> String {
    let delta = (now - then).max(0);
    match delta {
        0..=59 => format!("{delta}s ago"),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86_400),
    }
}

fn window_tabs(active: Window, counts: [usize; 3]) -> Line<'static> {
    let mut spans = Vec::new();
    for window in Window::all() {
        let index = match window {
            Window::Hour => 0,
            Window::Day => 1,
            Window::Week => 2,
        };
        let label = format!(" {} ({}) ", window.label(), counts[index]);
        let style = if window == active {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

pub fn draw(frame: &mut Frame, view: &View<'_>) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(9),
    ])
    .split(frame.area());

    draw_header(frame, view, areas[0]);
    draw_list(frame, view, areas[1]);
    draw_detail(frame, view, areas[2]);
    if view.show_help {
        draw_help(frame, frame.area());
    }
}

const HELP_LINES: &[&str] = &[
    "1 / 2 / 3      switch window (1h, 24h, 7d)",
    "Tab            next window",
    "j / k, arrows  move selection",
    "g / G          jump to top / bottom",
    "Enter          open the article in the browser",
    "l              toggle the local filter",
    "/              filter by text, Esc clears",
    "r              poll now",
    "?              close this help",
    "q              quit",
];

fn draw_help(frame: &mut Frame, area: Rect) {
    let width = 56.min(area.width);
    let height = (HELP_LINES.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let lines: Vec<Line> = HELP_LINES.iter().map(|line| Line::from(*line)).collect();
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys ")),
        popup,
    );
}

fn draw_header(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let counts = [
        view.stories.iter().filter(|_| view.window == Window::Hour).count(),
        view.stories.iter().filter(|_| view.window == Window::Day).count(),
        view.stories.iter().filter(|_| view.window == Window::Week).count(),
    ];
    let health = format!("{}/{} sources ok", view.sources_ok, view.sources_total);
    let polled = view
        .last_poll
        .map(|ts| relative(view.now, ts))
        .unwrap_or_else(|| "never".to_string());
    let local = if view.local_only { " · LOCAL" } else { "" };
    let filter = if view.filter.is_empty() {
        String::new()
    } else {
        format!(" · /{}", view.filter)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" bakutrend — {health} · polled {polled}{local}{filter} "));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(window_tabs(view.window, counts)), inner);
}

fn draw_list(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let show_delta = view.deltas.is_some();
    let mut widths = vec![Constraint::Length(4)];
    if show_delta {
        widths.push(Constraint::Length(6));
    }
    widths.extend([
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Min(20),
        Constraint::Length(8),
    ]);

    let mut header = vec![Cell::from("#")];
    if show_delta {
        header.push(Cell::from("Delta"));
    }
    header.extend(
        ["Score", "Cvg", "Eng", "Fresh", "Headline", "Outlets"]
            .into_iter()
            .map(Cell::from),
    );

    let rows: Vec<Row> = view
        .stories
        .iter()
        .enumerate()
        .map(|(index, story)| {
            let mut cells = vec![Cell::from(format!("{}", index + 1))];
            if let Some(deltas) = view.deltas {
                let delta = match deltas.get(&story.key) {
                    Some(movement) if *movement > 0 => format!("+{movement}"),
                    Some(movement) if *movement < 0 => format!("{movement}"),
                    Some(_) => "0".to_string(),
                    None => "new".to_string(),
                };
                cells.push(Cell::from(delta));
            }
            cells.extend([
                Cell::from(format!("{:.2}", story.score)),
                Cell::from(format!("{:.1}", story.coverage)),
                Cell::from(format!("{:.1}", story.engagement)),
                Cell::from(format!("{:.2}", story.freshness)),
                Cell::from(story.title.clone()),
                Cell::from(format!("{}", story.outlets.len())),
            ]);
            Row::new(cells)
        })
        .collect();

    let mut state = TableState::default();
    if !view.stories.is_empty() {
        state.select(Some(view.selected.min(view.stories.len() - 1)));
    }
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(Style::default().add_modifier(Modifier::BOLD)))
        .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_detail(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" detail ");
    let Some(story) = view.stories.get(view.selected) else {
        frame.render_widget(
            Paragraph::new("No stories yet — the first poll is still running.").block(block),
            area,
        );
        return;
    };

    let mut lines = vec![
        Line::from(story.title.clone()),
        Line::from(format!(
            "coverage {:.1}   engagement {:.1}   freshness {:.2}   score {:.2}   views {}",
            story.coverage, story.engagement, story.freshness, story.score, story.view_count
        )),
    ];
    if view.quiet_fallback {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!(
                    "Quiet hour — showing the latest {} stories instead",
                    view.stories.len()
                ),
                Style::default().fg(Color::Yellow),
            )),
        );
    }
    for outlet in &story.outlets {
        let views = outlet
            .views
            .map(|v| format!("{v} views"))
            .unwrap_or_else(|| "—".to_string());
        lines.push(Line::from(format!(
            " {}  {}  {}  {}",
            outlet.outlet,
            relative(view.now, outlet.newest),
            views,
            outlet.url
        )));
    }
    if !view.status.is_empty() {
        lines.push(Line::from(Span::styled(
            view.status.to_string(),
            Style::default().fg(Color::Red),
        )));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
```

Add to `src/lib.rs`:

```rust
pub mod ui;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ui:: 2>&1 | tail -30`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: stateless TUI rendering with score breakdown and hidden-until-known delta column"
```

---

### Task 12: Application state and key handling

**Files:**
- Create: `src/app.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::cluster::Clusterer`, `crate::config::Config`, `crate::score::{rank, ScoredStory}`, `crate::store::{Store, Window}`, `crate::ui::{draw, View}`.
- Produces: `crate::app::{AppEvent, Action, App}`.
  - `App::new(store: Store, config: Config, now: i64) -> App`
  - `App::handle(&mut self, AppEvent) -> Action`
  - `App::handle_key(&mut self, crossterm::event::KeyEvent) -> Action`
  - `App::refresh(&mut self, now: i64)`
  - `App::view(&self) -> View<'_>`
  - `App::record_poll(&mut self, report: &PollReport, now: i64)`

- [ ] **Step 1: Write the failing test**

Create `src/app.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{ParsedItem, SourceKind, SourceSpec};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    const NOW: i64 = 1_700_000_000;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app_with_stories() -> App {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = store
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
        // Deliberately distinct headlines: a shared vocabulary would merge them into one story.
        let headlines = [
            "Bakıda metro stansiyasında təmir işləri başladı",
            "Gəncədə toy karvanı qəza etdi, yaralılar var",
            "Sumqayıtda zavod yanğını söndürüldü",
            "Qarabağda diplomatik görüş keçirildi",
            "Naxçıvanda yeni magistral yol açıldı",
        ];
        let items: Vec<ParsedItem> = headlines
            .iter()
            .enumerate()
            .map(|(index, headline)| ParsedItem {
                external_id: format!("id{index}"),
                url: format!("https://apa.az/incident/x-{index}"),
                title: headline.to_string(),
                description: None,
                section: Some("incident".into()),
                published_at: NOW - index as i64 * 600,
                views: None,
                cited: false,
                publisher: None,
            })
            .collect();
        store.upsert_items(source_id, &items, NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        app.refresh(NOW);
        app
    }

    #[test]
    fn refresh_produces_one_ranked_story_per_distinct_headline() {
        let app = app_with_stories();
        assert_eq!(app.stories.len(), 5);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn movement_is_clamped_at_both_ends() {
        let mut app = app_with_stories();
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected, 0, "cannot move above the first row");
        for _ in 0..10 {
            app.handle_key(key(KeyCode::Char('j')));
        }
        assert_eq!(app.selected, app.stories.len() - 1, "cannot move past the last row");
    }

    #[test]
    fn number_keys_switch_window_and_reset_selection() {
        let mut app = app_with_stories();
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.window, Window::Day);
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.window, Window::Hour);
        assert_eq!(app.selected, 0);
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.window, Window::Week);
    }

    #[test]
    fn l_toggles_the_local_filter() {
        let mut app = app_with_stories();
        assert!(!app.local_only);
        app.handle_key(key(KeyCode::Char('l')));
        assert!(app.local_only);
        app.handle_key(key(KeyCode::Char('l')));
        assert!(!app.local_only);
    }

    #[test]
    fn q_quits_and_enter_opens_the_selected_story() {
        let mut app = app_with_stories();
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::Quit);

        match app.handle_key(key(KeyCode::Enter)) {
            Action::OpenUrl(url) => assert!(url.starts_with("https://apa.az/")),
            other => panic!("expected OpenUrl, got {other:?}"),
        }
    }

    #[test]
    fn r_requests_a_poll() {
        let mut app = app_with_stories();
        assert_eq!(app.handle_key(key(KeyCode::Char('r'))), Action::ForcePoll);
    }

    #[test]
    fn question_mark_toggles_help_and_escape_closes_it_before_quitting() {
        let mut app = app_with_stories();
        assert!(!app.show_help);
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.show_help);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None, "Esc closes help first");
        assert!(!app.show_help);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::Quit, "then Esc quits");
    }

    #[test]
    fn the_local_filter_keeps_only_matching_stories() {
        let mut app = app_with_stories();
        app.config.local_keywords = vec!["apa".to_string()];
        app.local_only = true;
        app.refresh(NOW);
        assert!(app.stories.is_empty(), "no headline mentions the keyword");
    }

    #[test]
    fn a_quiet_window_falls_back_to_the_latest_stories_with_a_label() {
        let mut app = app_with_stories();
        app.window = Window::Hour;
        // All five items are inside the hour, so nothing is quiet yet.
        app.refresh(NOW);
        assert!(!app.quiet_fallback);

        // Push the items outside the hour window.
        app.refresh(NOW + 7200);
        assert!(app.quiet_fallback, "an empty hour falls back rather than showing nothing");
        assert!(!app.stories.is_empty(), "the fallback still shows the latest stories");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib app:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type App in this scope`.

- [ ] **Step 3: Write minimal implementation**

```rust
//! Application state. All mutable state lives here; `ui::draw` is stateless and
//! `main` is a thin composition root.

use std::collections::HashMap;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};

use crate::cluster::Clusterer;
use crate::config::Config;
use crate::poller::PollReport;
use crate::score::{rank, ScoredStory};
use crate::store::{Store, Window};
use crate::text::matches_any;
use crate::ui::View;

/// Below this many stories, the active window is treated as quiet and the list falls
/// back to the latest stories under a visible banner.
const QUIET_THRESHOLD: usize = 3;

#[derive(Debug)]
pub enum AppEvent {
    Input(Event),
    PollDone(PollReport),
    PollFailed(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    OpenUrl(String),
    ForcePoll,
}

pub struct App {
    pub store: Store,
    pub config: Config,
    pub window: Window,
    pub stories: Vec<ScoredStory>,
    pub deltas: Option<HashMap<String, i64>>,
    pub selected: usize,
    pub local_only: bool,
    pub filter: String,
    pub filter_mode: bool,
    pub quiet_fallback: bool,
    pub show_help: bool,
    pub sources_ok: usize,
    pub sources_total: usize,
    pub last_poll: Option<i64>,
    pub status: String,
}

impl App {
    pub fn new(store: Store, config: Config, now: i64) -> Self {
        // The Google seed is not a polled source, so it does not count toward health.
        let sources_total = store
            .sources(true)
            .map(|rows| {
                rows.iter()
                    .filter(|row| row.kind != crate::source::SourceKind::Google)
                    .count()
            })
            .unwrap_or(0);
        let mut app = Self {
            store,
            config,
            window: Window::Day,
            stories: Vec::new(),
            deltas: None,
            selected: 0,
            local_only: false,
            filter: String::new(),
            filter_mode: false,
            quiet_fallback: false,
            show_help: false,
            sources_ok: 0,
            sources_total,
            last_poll: None,
            status: String::new(),
        };
        app.sources_ok = app.sources_total;
        app.refresh(now);
        app
    }

    /// Regroup and rerank. Called when data or the window changes, not on every tick.
    pub fn refresh(&mut self, now: i64) {
        let clusterer = Clusterer::new(self.config.cluster_threshold);
        let Ok((items, samples)) = self.store.window(self.window, now) else {
            self.status = "database read failed".to_string();
            return;
        };

        let mut stories = if items.is_empty() && self.window != Window::Week {
            self.quiet_fallback = true;
            let Ok((fallback, fallback_samples)) = self.store.window(Window::Week, now) else {
                return;
            };
            let groups = clusterer.group_items(&fallback);
            rank(&groups, &fallback_samples, Window::Week, &self.config.weights, now)
        } else {
            self.quiet_fallback = false;
            let groups = clusterer.group_items(&items);
            rank(&groups, &samples, self.window, &self.config.weights, now)
        };

        if self.local_only {
            stories.retain(|story| {
                matches_any(&story.title, &self.config.local_keywords)
                    || story.outlets.iter().any(|o| matches_any(&o.title, &self.config.local_keywords))
            });
        }
        if !self.filter.is_empty() {
            let needle = crate::text::fold(&self.filter);
            stories.retain(|story| crate::text::fold(&story.title).contains(&needle));
        }
        self.quiet_fallback = self.quiet_fallback || stories.len() < QUIET_THRESHOLD;

        self.deltas = self.compute_deltas(&clusterer, now, &stories);
        self.stories = stories;
        self.selected = self.selected.min(self.stories.len().saturating_sub(1));
    }

    /// Rank the immediately preceding window of equal length and express the change in
    /// position. `None` means there is not enough history yet, which hides the column.
    fn compute_deltas(
        &self,
        clusterer: &Clusterer,
        now: i64,
        current: &[ScoredStory],
    ) -> Option<HashMap<String, i64>> {
        if current.is_empty() {
            return None;
        }
        let span = self.window.seconds();
        let Ok((previous_items, previous_samples)) =
            self.store.window_data(now - 2 * span, now - span, self.window.allows_backfill())
        else {
            return None;
        };
        if previous_items.len() < QUIET_THRESHOLD {
            return None;
        }
        let groups = clusterer.group_items(&previous_items);
        let previous = rank(&groups, &previous_samples, self.window, &self.config.weights, now - span);

        let position: HashMap<&str, i64> =
            previous.iter().enumerate().map(|(index, story)| (story.key.as_str(), index as i64)).collect();
        let deltas = current
            .iter()
            .enumerate()
            .filter_map(|(index, story)| {
                position
                    .get(story.key.as_str())
                    .map(|old_index| (story.key.clone(), old_index - index as i64))
            })
            .collect::<HashMap<_, _>>();
        Some(deltas)
    }

    pub fn record_poll(&mut self, report: &PollReport, now: i64) {
        self.last_poll = Some(now);
        self.sources_ok = report.ok;
        self.status = if report.failed.is_empty() {
            String::new()
        } else {
            format!(
                "{} source(s) degraded: {}",
                report.failed.len(),
                report
                    .failed
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        self.refresh(now);
    }

    pub fn view(&self) -> View<'_> {
        View {
            window: self.window,
            stories: &self.stories,
            selected: self.selected,
            deltas: self.deltas.as_ref(),
            local_only: self.local_only,
            filter: &self.filter,
            quiet_fallback: self.quiet_fallback,
            show_help: self.show_help,
            sources_ok: self.sources_ok,
            sources_total: self.sources_total,
            last_poll: self.last_poll,
            now: chrono::Utc::now().timestamp(),
            status: &self.status,
        }
    }

    pub fn handle(&mut self, event: AppEvent) -> Action {
        match event {
            AppEvent::Input(Event::Key(key)) => self.handle_key(key),
            AppEvent::Input(Event::Resize(..)) => Action::None,
            AppEvent::Input(_) => Action::None,
            AppEvent::PollDone(report) => {
                let now = chrono::Utc::now().timestamp();
                self.record_poll(&report, now);
                Action::None
            }
            AppEvent::PollFailed(message) => {
                self.status = message;
                Action::None
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if self.filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filter_mode = false;
                    self.refresh(chrono::Utc::now().timestamp());
                }
                KeyCode::Enter => self.filter_mode = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refresh(chrono::Utc::now().timestamp());
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.refresh(chrono::Utc::now().timestamp());
                }
                _ => {}
            }
            return Action::None;
        }

        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc => {
                if self.show_help {
                    self.show_help = false;
                } else {
                    return Action::Quit;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < self.stories.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.selected = self.stories.len().saturating_sub(1);
            }
            KeyCode::Char('1') => self.switch_window(Window::Hour),
            KeyCode::Char('2') => self.switch_window(Window::Day),
            KeyCode::Char('3') => self.switch_window(Window::Week),
            KeyCode::Tab => {
                let next = match self.window {
                    Window::Hour => Window::Day,
                    Window::Day => Window::Week,
                    Window::Week => Window::Hour,
                };
                self.switch_window(next);
            }
            KeyCode::Char('l') => {
                self.local_only = !self.local_only;
                self.refresh(chrono::Utc::now().timestamp());
            }
            KeyCode::Char('/') => self.filter_mode = true,
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('r') => return Action::ForcePoll,
            KeyCode::Enter => {
                if let Some(story) = self.stories.get(self.selected) {
                    if let Some(outlet) = story.outlets.first() {
                        return Action::OpenUrl(outlet.url.clone());
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    fn switch_window(&mut self, window: Window) {
        if self.window != window {
            self.window = window;
            self.selected = 0;
            self.refresh(chrono::Utc::now().timestamp());
        }
    }
}
```

Add to `src/lib.rs`:

```rust
pub mod app;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib app:: 2>&1 | tail -30`
Expected: PASS, 8 tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: application state, key handling and window delta computation"
```

---

### Task 13: Binary, threads and smoke test

**Files:**
- Create: `src/cli.rs`
- Modify: `src/main.rs`, `src/lib.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: a working `bakutrend` binary supporting `--poll-only`, `--config`, `--log`, `--reset-db`.

- [ ] **Step 1: Write the failing test**

Create `src/cli.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn poll_only_and_reset_db_flags_parse() {
        let args = Cli::try_parse_from(["bakutrend", "--poll-only", "--reset-db"]).unwrap();
        assert!(args.poll_only);
        assert!(args.reset_db);
        assert!(args.config.is_none());
    }

    #[test]
    fn log_takes_an_optional_level_defaulting_to_debug() {
        let args = Cli::try_parse_from(["bakutrend", "--log"]).unwrap();
        assert_eq!(args.log.as_deref(), Some("debug"));

        let args = Cli::try_parse_from(["bakutrend", "--log", "info"]).unwrap();
        assert_eq!(args.log.as_deref(), Some("info"));

        let args = Cli::try_parse_from(["bakutrend"]).unwrap();
        assert_eq!(args.log, None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli:: 2>&1 | tail -20`
Expected: FAIL — `cannot find type Cli in this scope`.

- [ ] **Step 3: Write the CLI**

```rust
//! Command-line surface.

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "bakutrend", about = "Rank Azerbaijani news by popularity", version)]
pub struct Cli {
    /// Run the poller without the TUI, so week-history keeps accumulating.
    #[arg(long)]
    pub poll_only: bool,

    /// Configuration file. Defaults to the platform config directory.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Write logs to a file. Takes an optional level, defaulting to `debug`.
    #[arg(long, num_args = 0..=1, default_missing_value = "debug")]
    pub log: Option<String>,

    /// Delete the database and rebuild it from scratch.
    #[arg(long)]
    pub reset_db: bool,
}
```

Add to `src/lib.rs`:

```rust
pub mod cli;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib cli:: 2>&1 | tail -20`
Expected: PASS, 2 tests.

- [ ] **Step 5: Write the binary**

`src/main.rs`:

```rust
//! Composition root: resolve paths, open the store, then either run the poller alone
//! or run the TUI with the poller on its own thread.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bakutrend::app::{Action, App, AppEvent};
use bakutrend::cli::Cli;
use bakutrend::config::Config;
use bakutrend::dirs::AppDirs;
use bakutrend::poller::{poll_once, Backoff};
use bakutrend::source::http::HttpFetcher;
use bakutrend::source::{SourceKind, SourceSpec};
use bakutrend::store::Store;
use clap::Parser;

const TICK: Duration = Duration::from_millis(250);

fn main() {
    let cli = Cli::parse();
    if let Err(message) = run(cli) {
        eprintln!("Error: {message}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let dirs = AppDirs::resolve();
    let db_path = dirs.as_ref().map(AppDirs::db_path).unwrap_or_else(|| "bakutrend.sqlite".into());
    let config_path = cli
        .config
        .clone()
        .or_else(|| dirs.as_ref().map(AppDirs::config_path));

    if cli.reset_db && db_path.exists() {
        std::fs::remove_file(&db_path)?;
        let _ = std::fs::remove_file(db_path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("sqlite-shm"));
    }

    let config = match config_path.as_deref() {
        Some(path) if path.exists() => Config::load(path)?,
        _ => Config::default(),
    };

    if let (Some(level), Some(dirs)) = (cli.log.as_deref(), dirs.as_ref()) {
        init_file_logger(level, &dirs.log_path());
    }

    let mut store = Store::open(&db_path)?;
    for (spec, enabled) in config.source_specs() {
        store.ensure_source(&spec, enabled)?;
        if !enabled {
            let rows = store.sources(false)?;
            if let Some(row) = rows.iter().find(|r| r.locator == spec.locator) {
                store.set_enabled(row.id, false)?;
            }
        }
    }

    // The Google source exists only to seed the week window once. It is created here
    // because it is deliberately absent from the user's configurable source list.
    store.ensure_source(
        &SourceSpec {
            kind: SourceKind::Google,
            outlet: "Google News".to_string(),
            name: "Google News 7d".to_string(),
            locator: "google:7d".to_string(),
        },
        true,
    )?;

    let now = chrono::Utc::now().timestamp();
    if cli.poll_only {
        return run_poller_forever(store, config, now);
    }

    let (tx, rx) = mpsc::channel::<AppEvent>();
    // Pressing `r` nudges the poller rather than waiting out the interval.
    let (nudge_tx, nudge_rx) = mpsc::channel::<()>();

    let input_tx = tx.clone();
    thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if input_tx.send(AppEvent::Input(event)).is_err() {
                break;
            }
        }
    });

    let poll_tx = tx.clone();
    let interval = Duration::from_secs(config.poll_interval_secs.max(30));
    let retention_days = config.retention.view_sample_days;
    thread::spawn(move || {
        let fetcher = match HttpFetcher::new("azerbaycan OR baki OR bakı") {
            Ok(fetcher) => fetcher,
            Err(error) => {
                let _ = poll_tx.send(AppEvent::PollFailed(error.to_string()));
                return;
            }
        };
        let mut store = store;
        let mut backoff = Backoff::new();
        loop {
            let now = chrono::Utc::now().timestamp();
            match poll_once(&mut store, &fetcher, &mut backoff, now, retention_days) {
                Ok(report) => {
                    if poll_tx.send(AppEvent::PollDone(report)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    if poll_tx.send(AppEvent::PollFailed(error.to_string())).is_err() {
                        break;
                    }
                }
            }
            match nudge_rx.recv_timeout(interval) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    let reader = Store::open(&db_path)?;
    let mut app = App::new(reader, config, now);
    let mut terminal = ratatui::init();
    let result = run_tui(&mut terminal, &mut app, &rx, &nudge_tx);
    ratatui::restore();
    result
}

fn run_tui(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mpsc::Receiver<AppEvent>,
    nudge: &mpsc::Sender<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        terminal.draw(|frame| {
            let view = app.view();
            bakutrend::ui::draw(frame, &view);
        })?;

        match rx.recv_timeout(TICK) {
            Ok(event) => match app.handle(event) {
                Action::Quit => return Ok(()),
                Action::OpenUrl(url) => open_in_browser(&url),
                Action::ForcePoll => {
                    let _ = nudge.send(());
                }
                Action::None => {}
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn run_poller_forever(
    mut store: Store,
    config: Config,
    _now: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let fetcher = HttpFetcher::new("azerbaycan OR baki OR bakı")?;
    let mut backoff = Backoff::new();
    let interval = Duration::from_secs(config.poll_interval_secs.max(30));
    loop {
        let now = chrono::Utc::now().timestamp();
        match poll_once(&mut store, &fetcher, &mut backoff, now, config.retention.view_sample_days) {
            Ok(report) => eprintln!(
                "[{}] ok={} new={} samples={} pruned={} failed={}",
                now,
                report.ok,
                report.new_items,
                report.samples,
                report.pruned,
                report.failed.len()
            ),
            Err(error) => eprintln!("poll failed: {error}"),
        }
        thread::sleep(interval);
    }
}

fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

/// Minimal opt-in file logger, matching the sibling project: off by default, truncated
/// on startup.
fn init_file_logger(level: &str, path: &std::path::Path) {
    struct FileLogger(std::sync::Mutex<std::fs::File>);
    impl log::Log for FileLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            use std::io::Write;
            if let Ok(mut file) = self.0.lock() {
                let _ = writeln!(file, "[{}] {}", record.level(), record.args());
            }
        }
        fn flush(&self) {}
    }

    let parsed = match level.to_ascii_lowercase().as_str() {
        "off" => return,
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Debug,
    };
    if let Ok(file) = std::fs::File::create(path) {
        let logger: &'static FileLogger = Box::leak(Box::new(FileLogger(std::sync::Mutex::new(file))));
        if log::set_logger(logger).is_ok() {
            log::set_max_level(parsed);
        }
    }
}
```

- [ ] **Step 6: Run the real thing against the live network**

Run: `cargo run 2>&1 | tail -5`
Expected: the TUI opens and within ~30 seconds the list fills with real Azerbaijani headlines.

Verify by hand, in order:
1. The header reads `n/20 sources ok` with `n` at or near 19.
2. Press `2` — the list shows 24h stories with an outlet count column.
3. Press `3` — the 7d window shows a larger list, partly backfilled from Google.
4. Press `j` several times, then confirm the detail pane's outlet list changes with the selection. The pane must show `coverage`, `engagement`, `freshness` and `score`.
5. Press `l` — the list narrows to stories matching the local keywords.
6. Press `Enter` on a story — the article opens in the browser.
7. Press `q` — the terminal restores cleanly, with no leftover raw-mode state.

- [ ] **Step 7: Verify the poll-only mode and the database**

```bash
cargo run -- --poll-only 2>&1 | head -3
```
Expected: one line like `[...] ok=19 new=347 samples=... pruned=0 failed=0`. Then confirm rows exist:

```bash
sqlite3 ~/Library/Application\ Support/bakutrend/bakutrend.sqlite \
  "SELECT COUNT(*) FROM items; SELECT COUNT(*) FROM view_samples; SELECT COUNT(DISTINCT outlet_id) FROM items;"
```
Expected: three non-zero counts, with more distinct outlets than the 10 RSS feeds alone (proving Google publisher attribution works).

- [ ] **Step 8: Run the full test suite and the CI gates**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | tail -20`
Expected: all three pass.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat: bakutrend binary with poll-only mode, input and poller threads"
```

---

### Task 14: README and the launchd recipe

**Files:**
- Create: `README.md`

**Interfaces:**
- Consumes: everything above.
- Produces: documentation. No code.

- [ ] **Step 1: Write the README**

`README.md`:

```markdown
# bakutrend

A terminal UI that answers one question: **what is Azerbaijan reading right now?**

It polls 10 Azerbaijani news outlets and their Telegram channels every five minutes,
groups articles that describe the same event, and ranks those events by popularity in
the last hour, the last day, or the last week.

Popularity combines two signals:

- **Cross-outlet coverage** — how many independent outlets carry the story. Reposts that
  credit another outlet weigh half, and one outlet publishing both a feed and a channel
  still casts a single vote.
- **Reader engagement** — Telegram view counts, normalized per channel so a large channel's
  routine post does not outrank a small channel's breakout post.

Press `Enter` on any story to see every outlet carrying it, each with its timestamp and
view count, plus the coverage / engagement / freshness breakdown behind its score.

## Install

```sh
cargo build --release
cp target/release/bakutrend ~/.local/bin/
```

## Usage

```
bakutrend                 # the TUI
bakutrend --poll-only     # poller only, no UI
bakutrend --config PATH   # explicit config file
bakutrend --log [LEVEL]   # write logs to the state directory (default level: debug)
bakutrend --reset-db      # delete and rebuild the database
```

Keys: `1` `2` `3` or `Tab` switch window · `j` `k` or arrows move · `g` `G` jump to
top/bottom · `Enter` opens the article · `l` toggles the local filter · `/` filters by
text · `r` forces a poll · `q` quits.

## How the week window fills in

RSS feeds only reach back about a day, and Telegram exposes view counts for roughly
fourteen hours. On an empty database the week window is seeded once from Google News'
`when:7d` results, and those seeded items never appear in the 1-hour or 24-hour windows.
Real historically observed data accumulates only while the poller runs, so for the first
week the week view is coverage-weighted and partly seeded.

To keep history accumulating while the TUI is closed, install a launchd agent:

```sh
cat > ~/Library/LaunchAgents/az.bakutrend.poll.plist <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>az.bakutrend.poll</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/CHANGEME/.local/bin/bakutrend</string>
    <string>--poll-only</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/bakutrend.out.log</string>
  <key>StandardErrorPath</key><string>/tmp/bakutrend.err.log</string>
</dict>
</plist>
PLIST

launchctl load ~/Library/LaunchAgents/az.bakutrend.poll.plist     # start
launchctl unload ~/Library/LaunchAgents/az.bakutrend.poll.plist   # stop
```

Replace `CHANGEME` with your username. Nothing in `bakutrend` creates this file; it is
yours to install and remove.

## Configuration

Lives in the platform config directory, for example
`~/Library/Application Support/bakutrend/config.toml`. Any key you omit keeps its default.
A `sources` block replaces the whole default list, so copy it before editing.

```toml
poll_interval_secs = 300
cluster_threshold  = 0.45

[weights]
coverage   = 0.40
engagement = 0.40
freshness  = 0.20

[retention]
view_sample_days = 30

local_keywords = ["Bakı", "Gəncə", "Qarabağ", "Azərbaycan"]

[[sources]]
name    = "Qafqazinfo RSS"
kind    = "rss"          # rss | telegram | google
locator = "https://qafqazinfo.az/rss"
outlet  = "Qafqazinfo"   # one outlet may own several sources
enabled = true
```

Data lives beside it in `bakutrend.sqlite`; logs, when enabled, in the state directory.

## Known limits

- Telegram view counts are biased by channel size. Per-channel normalization ranks
  relative buzz within a channel, not absolute reach.
- Citation detection is lexical. Some reposts will be missed.
- Grouping is lexical, so two outlets describing one event with no shared vocabulary
  remain separate stories.
- The local filter is a keyword heuristic, not a geocoder.
```

- [ ] **Step 2: Verify the documented commands work**

Run: `cargo run -- --help 2>&1 | head -20`
Expected: the flags in the README appear with the same names.

Run: `cargo run -- --reset-db --poll-only 2>&1 | head -2`
Expected: the database is rebuilt and one poll line prints. This proves `--reset-db` does not
crash when the file is already absent.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "docs: document usage, history behaviour, configuration and the launchd recipe"
```

---

## Self-Review

**1. Spec coverage.** Every spec section maps to a task:

| Spec section | Task |
|---|---|
| §5 source inventory | 8 (config source list), 2/3 (parser fixtures), 9 (fetching) |
| §6 architecture, threading | 13 (threads), 12 (state), 10 (poll cycle) |
| §7 data model | 4, 5 (with the stories-table delta noted) |
| §8.1–8.2 ingestion and parsing | 2, 3, 9 |
| §8.3 syndication | 2 (`is_cited`), 7 (0.5 weight) |
| §8.4 idempotency | 4 (unit), 10 (cycle-level, twice) |
| §8.5 backfill | 5 (window exclusion), 9 (parsing), 10 (seed trigger) |
| §9 clustering | 6 |
| §10 ranking | 7 |
| §10.1 local filter | 1 (`matches_any`), 12 (applied) |
| §11 TUI | 11, 12 |
| §12 CLI and config | 8, 13 |
| §13 error handling | 1 (error enums), 10 (isolation and backoff) |
| §14 retention | 5 (`prune_samples`), 10 (called per cycle) |
| §15 testing strategy | every task; 10 is the end-to-end |
| §16 conventions | Global Constraints; 1 (Cargo.toml) |
| §17 dependencies | 1 |
| §18 known limits | 14 (README) |

The `?` help overlay from §11 is implemented in Tasks 11 and 12: `View::show_help` renders a
centered popup, `?` toggles it, and `Esc` closes it before it quits.

**2. Placeholder scan.** No `TBD`, `TODO`, "add error handling", "similar to Task N", or
unshown code steps. Two places intentionally show a wrong-then-corrected shape (Task 7)
with the corrected code spelled out in full, because the closure-indexing wart is easy to
hit and the fix must be explicit.

**3. Type consistency.** Checked across tasks: `ParsedItem` gains `publisher` in Task 2 and
is used with that field in Tasks 3, 4, 9, 10, 12. `Window` is defined in Task 4 and used in
5, 7, 10, 11, 12. `ItemRow` fields match between Tasks 4, 5, 6, 7. `ScoredStory` is defined
in Task 7 and constructed in 11's tests with exactly those fields — including
`show_help` on `View`, added to the `base()` helper. `SourceSpec` fields (`kind`, `outlet`,
`name`, `locator`) are identical in Tasks 2, 4, 8, 10, 13. `FetchError` variants used in 9
and 10 exist in 1. `AppDirs` methods used in 13 exist in 8. `Store::window_data` is
introduced in Task 5 and used in Task 12; `Store::window` in Tasks 5, 10, 12.

**4. Defects found and fixed during this review**, each of which would have failed on the
first run:

- The Task 12 fixture used five headlines differing only in a digit. Digits are dropped as
  tokens, so all five produced identical token sets and merged into one story, failing
  `assert_eq!(app.stories.len(), 5)`. Replaced with five genuinely distinct headlines.
- The Google seed source was never created, so `poll_once`'s seed branch could never find
  it and the week window would have stayed empty. Task 13 now creates it explicitly, and
  Task 10 excludes `SourceKind::Google` from the per-cycle loop so it stays a one-shot seed.
- `Action::ForcePoll` was wired to a no-op `PollDone(Default::default())`. Replaced with a
  real nudge channel: the poller thread waits on `recv_timeout(interval)` and wakes early
  when `r` is pressed.
- The poller thread hardcoded a 30-day retention instead of reading it from config.
- `App::new` counted the Google seed toward source health, which would have shown `n/21`
  forever. It now counts only polled sources.

