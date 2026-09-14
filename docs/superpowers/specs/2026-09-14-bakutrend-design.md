# bakutrend — Design Specification

**Status:** approved for planning
**Date:** 2026-09-14
**Author:** brainstorming session (Pocock /to-prd + /grill-me)

---

## 1. Summary

`bakutrend` is a terminal user interface that answers one question: **what is Azerbaijan reading right now?** It continuously polls Azerbaijani news outlets and their Telegram channels, groups articles that describe the same event, and ranks those events by popularity in a chosen time window — the last hour, the last 24 hours, or the last week.

Popularity is measured two ways, combined into one score:

- **Cross-outlet coverage** — how many independent outlets are carrying the story.
- **Reader engagement** — Telegram view counts on the same story, normalized per channel.

One binary, `bakutrend`. Running it opens the TUI and starts the poller. Running it with `--poll-only` runs only the poller.

---

## 2. Problem and motivation

Azerbaijani news is fragmented: a dozen outlets publish fast, syndicate heavily from each other, and mix local reporting with reposted world news. Telegram is where a large share of actual reading happens, and it exposes view counts — a direct attention signal that no RSS feed provides.

Existing news readers give a chronological firehose, not a ranked answer. Google News ranks editorially, not by reader attention, and is licensed for personal feed-reader use only. Neither tells you *what is big in Azerbaijan right now*.

`bakutrend` exists to produce that ranked answer, with the evidence for each ranking visible.

---

## 3. Goals

- Rank stories by popularity in three windows: 1 hour, 24 hours, 7 days.
- Combine outlet breadth with real reader attention.
- Show *why* a story ranked where it did: coverage, engagement, freshness, per-outlet breakdown.
- Survive flaky sources: one dead feed never blocks the other nineteen.
- Accumulate honest history for the week window, rather than faking it.
- Stay explainable: no opaque model, no black-box ranking.

## 4. Non-goals

- No translation. Headlines render as published (Azerbaijani, Russian or English); UI chrome is English.
- No machine learning, embeddings or vector search.
- No non-interactive **ranking output** mode: no `--top`, no JSON on stdout. Ranking is a pure function, so a printer can be added later without rework. `--poll-only` (§12) is not an exception — it produces no ranking output, it only keeps history accumulating.
- No category filtering UI, no notifications, no article full-text reading, no images.
- No reading of article bodies. Only feed metadata, Telegram post text and view counts.
- No mobile, web or server component.

---

## 5. Verified source inventory

Every entry below was fetched successfully during design research. All 20 sources ship **enabled by default** and every one is individually toggleable in config.

### 5.1 RSS feeds (10)

| Outlet | URL | Items | Depth | Local/world | Language |
|---|---|---|---|---|---|
| Qafqazinfo | `https://qafqazinfo.az/rss` | 100 | 45 h | 40/60 | az |
| APA | `https://apa.az/rss` | 50 | 26 h | 45/55 | az |
| Azertag | `https://azertag.az/rss` | 50 | 15 h | 50/50 | az |
| Report | `https://report.az/rss/` | 30 | 7 h | 50/50 | az |
| Modern.az | `https://modern.az/rss` | 30 | 8 h | 50/50 | az |
| Olke.az | `https://olke.az/rss` | 30 | 5 h | 45/55 | az |
| Baku.ws | `https://baku.ws/rss` | 30 | 7 h | 20/80 | az |
| Trend.az | `https://trend.az/rss/` | 25 | 19 h | 15/85 | en |
| Minval | `https://minval.az/rss` | 23 | 10 h | 15/85 | ru |
| Haqqin.az | `https://haqqin.az/rss/` | 20 | 6 h | 25/75 | ru |

### 5.2 Telegram channels (10)

All are read through the public preview at `https://t.me/s/<handle>`; all expose view counts.

| Handle | Posts/page | Span | Local/world | Notes |
|---|---|---|---|---|
| `@qafqazinfo` | 20 | 14 h | 40/60 | highest views (2.2–2.7 K) |
| `@apa_az` | 20 | 10 h | 45/55 | |
| `@dayaz` | 20 | 27 h | 45/55 | |
| `@axaraz` | 19 | 21 h | 65/35 | most Baku-local |
| `@minval_az` | 21 | 10 h | 45/55 | best views of news set |
| `@reportnewsaz` | 21 | 8 h | 40/60 | pairs with report.az RSS |
| `@apatv` | 20 | 30 h | 55/45 | ships disabled — see below |
| `@bakupost` | 17 | 30 h | 70/30 | small channel (35–43 views) |
| `@qaynarinfo` | 13 | 32 h | 50/50 | |
| `@meydantv` | 20 | 92 h | 50/50 | 1–2 posts/day; 7 d window only |

`@apatv` answered normally during this research but by the end of the session returned the
preview-less ~9.5 KB stub, twice, minutes apart. It therefore ships with `enabled = false`
and a dated comment in the default config, and the enabled count is 19 of 20. This is also
the reason an empty preview is treated as a **fetch failure** rather than an empty channel:
HTTP 200 with zero posts means the channel disabled previews, and silently reporting that as
"0 new items" would hide a dead source.

### 5.3 Rejected during research

- `turan.az/rss`, `qaynarinfo.az/rss` — Cloudflare challenge, not fetchable by plain HTTP client.
- `axar.az/rss` — HTTP 404.
- `1news.az/rss`, `news.az/rss`, `caliber.az/rss` — return HTML, no feed.
- `news.day.az/rss` — HTTP 410.
- `@azernews`, `@azerbaycan24` — channels exist but stale (last post Aug 2026 / Apr 2026).
- Roughly 100 other Telegram handles exist as ~9.6 KB stubs with no preview.

### 5.4 Parser traps (verified, not hypothetical)

- `qafqazinfo` emits the **same `<guid>` for every item** (`https://www.qafqazinfo.az/news/detail/`, slug stripped). Keying on `guid` would collapse the whole feed into one article.
- `azertag` emits **no `<guid>` at all**.
- `modern.az` embeds HTML entities (`&nbsp;`, `&ldquo;`) in titles.
- `qafqazinfo` nests CDATA inside escaped text; its descriptions are truncated mid-word.

Consequence: the item identity key is, in priority order, the item `<link>`, then a hash of the title. Never `guid` alone.

---

## 6. Architecture

One binary, one crate, modules rather than a workspace. The sibling project `ttymap` splits into seven crates because it has a plugin runtime and a two-process engine; `bakutrend` has neither and does not pay that cost.

```
src/
  main.rs          CLI flags, composition root, thread wiring
  cli.rs           clap argument definitions
  config.rs        config file load, defaults, source toggles
  dirs.rs          config/data/state directories
  error.rs         thiserror error enums, one per boundary
  text.rs          fold, entity decoding, tokenization, keyword matching
  source/
    mod.rs         SourceKind, ParsedItem, ParseOutcome, citation detection
    rss.rs         feed-rs wrapper for the 10 feeds
    telegram.rs    t.me/s/<handle> HTML extraction
    google.rs      when:7d backfill (one-shot seed)
    http.rs        Fetcher trait and its HTTP implementation
  store.rs         SQLite schema, upsert, windowed queries, pruning
  cluster.rs       normalization, token overlap, story assignment
  score.rs         windowed ranking
  poller.rs        poll cycle, per-source backoff
  ui.rs            stateless draw function
  app.rs           App state, AppEvent handling, key dispatch
```

Three modules beyond the first draft, each with one responsibility: `text.rs` (normalization
is shared by clustering, citation detection and the local filter, so it cannot live inside
any one of them), `poller.rs` (the poll cycle takes a `Fetcher`, so it can be driven offline
by fixtures), and `source/http.rs` (the network boundary, separate from the parsing).

### 6.1 Threading

Three threads, one channel.

- **Input thread** — blocks on `crossterm::event::read()`, pushes `AppEvent::Input`.
- **Poller thread** — loops every 5 minutes, fetches all enabled sources sequentially with a per-source timeout, writes to SQLite, pushes `AppEvent::PollDone(PollReport)`.
- **Main thread** — owns `App`, blocks on `recv_timeout(250 ms)`. The timeout doubles as the UI tick that advances relative timestamps and re-queries the ranking.

One channel carries outbound `AppEvent` values from every producer; `App` is the single owner of mutable state; `ui::draw` is a stateless free function taking a snapshot. This is the transferable core of `ttymap`'s pattern, minus its compositor, focus stack, IPC split and Lua runtime.

Channel is `std::sync::mpsc`; `Sender` is cloned per producer. `crossbeam-channel` is not needed.

### 6.2 Storage concurrency

SQLite in WAL mode. The poller thread owns all writes. The UI opens a second read-only connection. A slow render never blocks a poll, and a poll never blocks a keystroke.

---

## 7. Data model

```sql
PRAGMA journal_mode = WAL;

CREATE TABLE outlets (              -- publisher identity, one row per organisation
  id      INTEGER PRIMARY KEY,
  name    TEXT NOT NULL UNIQUE,
  host    TEXT                     -- site host, for matching Google backfill entries
);

CREATE TABLE sources (              -- one row per feed or channel
  id       INTEGER PRIMARY KEY,
  outlet_id INTEGER NOT NULL REFERENCES outlets(id),
  kind     TEXT NOT NULL,          -- 'rss' | 'telegram' | 'google'
  name     TEXT NOT NULL,
  locator  TEXT NOT NULL,          -- feed URL or telegram handle
  enabled  INTEGER NOT NULL DEFAULT 1,
  UNIQUE(kind, locator)
);

CREATE TABLE items (
  id           INTEGER PRIMARY KEY,
  source_id    INTEGER NOT NULL REFERENCES sources(id),
  external_id  TEXT NOT NULL,      -- link, post id, or title hash
  url          TEXT NOT NULL,
  title        TEXT NOT NULL,
  description  TEXT,
  section      TEXT,               -- URL slug, stored but not surfaced
  published_at INTEGER NOT NULL,   -- unix seconds, UTC
  first_seen   INTEGER NOT NULL,
  last_seen    INTEGER NOT NULL,
  views        INTEGER,            -- Telegram only, NULL otherwise
  cited        INTEGER NOT NULL DEFAULT 0,  -- syndicated repost
  is_backfill  INTEGER NOT NULL DEFAULT 0,  -- from the Google 7 d seed
  UNIQUE(source_id, external_id)
);
CREATE INDEX items_published ON items(published_at);
CREATE INDEX items_source    ON items(source_id, published_at);

CREATE TABLE view_samples (         -- one row per Telegram item per poll
  item_id INTEGER NOT NULL REFERENCES items(id),
  ts      INTEGER NOT NULL,
  views   INTEGER NOT NULL,
  PRIMARY KEY (item_id, ts)
);
```

**Stories are not persisted.** Grouping is a pure function recomputed in memory from the
last seven days of `items` — roughly 10k rows, milliseconds of work. An earlier draft
carried `stories` and `story_items` tables; deriving stories instead removes an entire
class of incremental-merge bugs and lets `cluster::group_items` be tested with no database
at all. The delta column already needed no extra storage, so nothing else changed.

**Why `outlets` is separate from `sources`.** `qafqazinfo` and `report.az` each publish both an RSS feed and a Telegram channel. Counting sources as coverage would let one outlet cast two votes for the same story. Coverage counts **distinct outlets**.

**Why `view_samples` exists.** Telegram view counts are readable only while the post sits in the ~14 h preview window; once it scrolls off, the number is gone forever. Velocity — views gained per hour — is a far better popularity signal than a raw total, and it cannot be reconstructed after the fact. Samples are captured on every poll.

---

## 8. Ingestion

### 8.1 Fetching

Sequential, one source at a time, per-source timeout of 15 s, browser `User-Agent` header (mandatory — `azertag.az` returns HTTP 400 without it). A 5-minute interval across 20 sources is 240 requests/hour in total, so concurrency would add machinery for no benefit.

### 8.2 Parsing

- **RSS** — `feed-rs`, which normalizes titles (entity and CDATA decoding) and tolerates the missing and duplicated fields described in §5.4.
- **Telegram** — `scraper` (html5ever) over the preview HTML. Extract `data-post` (unique id), `<time datetime>`, `.tgme_widget_message_views` text, and `.tgme_widget_message_text`. View strings arrive as `2.65K` / `1.1K` / `340` and are parsed to integers. Media-only posts without text are stored with `title` taken from any caption, or skipped if there is genuinely none.
- **Google News** — `feed-rs` over `https://news.google.com/rss/search?q=…&when:7d`. Titles are stripped of the trailing ` - Publisher`. The publisher comes from the RSS `<source>` element, which `feed-rs` exposes as a **name only, with no URL**, so attribution matches on the folded name rather than the host: exact match after folding, else a prefix match when the shorter name is at least four characters, so Google's `Report.az` lands on the configured `Report` outlet. This affects the week-window seed only.

### 8.3 Syndication detection

Azerbaijani outlets repost each other constantly. During research, `qafqazinfo` was observed publishing "APA-ya istinadən xəbər verir ki…". Five outlets reposting one APA story is editorial reach, not five independent confirmations.

Detection is a regex over the description for citation markers: `istinadən`, `istinadla`, `-a istinadla`, `-yə istinadla`, `məlumatına görə`, plus explicit outlet names. A flagged item sets `cited = 1`.

**Correction, made after implementation:** the shipped detector matches citation MORPHOLOGY only
and does NOT treat a bare outlet name as a marker. Matching a name on its own would halve the
coverage weight of any story that merely mentions an outlet — a far larger corruption of the
central signal than the reposts it would catch. An outlet-name marker is only correct in a form
that already carries a citation verb, which the morphological markers above already cover. The
detector is shared by all three source kinds, so an Azerbaijani Telegram repost is discounted the
same way an article feed's is.

Effect on coverage: for each outlet, its weight is `1.0` if it has any uncited item in the window, otherwise `0.5`. An outlet's weight is taken once (the maximum over its items), never summed per item.

### 8.4 Idempotency

`UNIQUE(source_id, external_id)` with `INSERT … ON CONFLICT DO UPDATE` setting `last_seen` and refreshing `views`. Polling the same unchanged feed twice must not change any ranking input. This is a tested invariant.

### 8.5 Backfill

When the 7-day window holds no locally observed data, the poller performs one Google News `when:7d` seed, marking those items `is_backfill = 1`. Backfill items:

- are **excluded** from the 1 h and 24 h windows entirely — local polling is fresher and richer there, and backfill items carry no view counts;
- are **included** in the 7 d window, and are progressively displaced as locally observed data accumulates.

---

## 9. Clustering

Two items belong to the same story when they describe the same event.

**Normalization.** Lowercase; strip Azerbaijani diacritics (`ə→e`, `ı→i`, `ş→s`, `ğ→g`, `ç→c`, `ö→o`, `ü→u`; Russian text is left as-is but lowercased); drop punctuation; remove a small Azerbaijani/Russian stopword list; keep tokens of 4 or more characters; also keep proper-noun runs as single tokens.

**Similarity.** Jaccard similarity over token sets. Match when similarity ≥ `cluster.threshold` (default `0.45`) or when two items share ≥ 2 distinctive proper nouns. The threshold lives in config so real data tunes it.

**Assignment.** New items are compared against stories whose `last_published` falls inside the largest window (7 days). An in-memory inverted index from token to story id means only stories sharing at least one token are compared, rather than every story in the window. On a match the item joins that story and `last_published` advances; otherwise a new story is created with the item's token signature as its `key`.

**Representative title.** The title of the story's earliest item, so a story that evolves keeps a stable heading.

---

## 10. Ranking

For each window $W \in \{1h, 24h, 7d\}$, taking `now` as an injected parameter so ranking is deterministic under test:

$$\text{score} = 0.40 \cdot \widehat{C} + 0.40 \cdot \widehat{E} + 0.20 \cdot F$$

**Candidates.** Stories with at least one contributing item whose `published_at` falls inside $W$, subject to the backfill rule in §8.5.

**Coverage $C$.** Sum over distinct outlets of the outlet weight from §8.3. Normalized: $\widehat{C} = C / \max(C)$ over candidates, guarded against a zero maximum.

**Engagement $E$.** For each Telegram item in $W$:

- `views_per_hour` = (latest sample − first sample) / hours elapsed, when at least two samples exist and at least 10 minutes apart;
- otherwise the item's total views divided by its age in hours, floored at a quarter hour.

Both branches therefore measure the same quantity, views per hour. An earlier draft used
the raw view count in the second branch, which mixed units inside a single sum and made the
engagement term incoherent.

Per-channel normalization divides that by the median `views_per_hour` of the same channel's items in $W$ (floor of 1, to avoid dividing by zero). This is what stops `@qafqazinfo`'s 2.65 K views from automatically beating `@dayaz`'s 340 — without it, the ranking measures subscriber counts, not stories.

$E$ is the sum over the story's Telegram items; $\widehat{E}$ normalizes against the window maximum.

**Freshness $F$.** $F = \exp(-\text{age}_h / (W_h/3))$, where age is measured from the story's newest item. A story decays within its own window.

**Delta.** Each story's rank in $W$ is compared with its rank in the immediately preceding window of equal length (computed from `items`, needing no extra storage). The delta column is **hidden entirely** until that preceding window holds enough data to rank; then it renders `+3`, `-1`, or `new`. A column of dashes teaches nothing and costs width.

**Explainability.** The detail pane displays $C$, $E$, $F$ and the final score for the selected story. When a ranking looks wrong, the cause is visible rather than guessed at.

### 10.1 Local filter

A config-editable keyword list matched against normalized title and description: place names (Bakı, Gəncə, Sumqayıt, Mingəçevir, Bərdə, Lənkəran, Astara, Naxçıvan, Şuşa, Xankəndi, Qarabağ, Şərqi Zəngəzur, Xəzər, Abşeron …) and institutions (Azərbaycan, Prezident, Milli Məclis, XİN, Nazirlər Kabineti, DİN, SOCAR, AZAL, ADY, ANAMA …).

Toggled with `l`, off by default. It is a **heuristic**: the default view is everything Azerbaijani media publishes, because world news carried by all ten outlets genuinely is what Baku is reading — that is an answer, not noise. The filter is one keypress away, and it is never applied silently.

---

## 11. TUI

```
┌ bakutrend ───────────────────────── 12/20 sources ok · polled 34s ago ┐
│ [1h]  24h   7d   ·  [Local]  ·  / filter                                │
├─────────────────────────────────────────────────────────────────────────┤
│ #   Δ     Score  Cvg  Eng  Fresh  Headline                      Outlets │
│ 1   +3    0.91   7.0  1.8  0.72   Bakıda bu yollar bağlıdır         7   │
│ 2   new   0.84   3.0  2.4  0.95   Gəncədə iki nəfər bıçaqlandı      3   │
├─────────────────────── detail ──────────────────────────────────────────┤
│ Bakıda bu yollar bağlıdır — Bakı Nəqliyyat Agentliyi məlumat yayıb…     │
│ coverage 7.0   engagement 1.8   freshness 0.72   score 0.91             │
│  qafqazinfo    20:31  2.65K views   qafqazinfo.az/news/detail/…         │
│  apa.az        20:12  1.10K views   apa.az/incident/…                   │
└─────────────────────────────────────────────────────────────────────────┘
```

**Keys.** `1` / `2` / `3` or `Tab` switch windows; `j` / `k` or arrows move; `g` / `G` jump to top or bottom; `Enter` opens the article in the default browser (`open`); `l` toggles the local filter; `/` filters by text; `r` forces a poll; `?` shows help; `q` quits.

**Default window on open:** 24 hours — the 1 h window is often thin and the 7 d window is initially backfilled.

**Empty state.** If the active window holds fewer than 3 stories, the list shows the most recent stories under a visible banner: `Quiet hour — showing the latest 12 stories instead`. The app never silently displays a different time range than the one it claims.

**Degradation.** The header reports `n/N sources ok`, where N counts the ENABLED, non-Google
sources — 19 with the shipped defaults, 20 if `@apatv` is re-enabled, and anything else for a
custom source list. A deliberately disabled source cannot be "ok", so counting it would pin the
header below full forever and read as a permanent failure; the Google seed is excluded because it
is never polled per cycle. Before the first poll completes, health is unknown and the header says
so rather than claiming every source is up. A source that has failed repeatedly backs off
exponentially up to 30 minutes and is named in the help overlay, so a quietly broken feed is
discoverable rather than invisible.

**Text.** Headlines render as published in Azerbaijani, Russian or English. Truncation uses `unicode-width`, never byte or `char` counts.

---

## 12. CLI and configuration

```
bakutrend [--poll-only] [--config <path>] [--log [LEVEL]] [--reset-db]
```

- `--poll-only` runs the poller without the TUI, so week-history accumulates while the TUI is closed. No launchd agent is installed; the README documents an example plist for the user to install if they want it.
- `--log [LEVEL]` installs the opt-in file logger, matching `ttymap`'s convention: default off, output to a state-directory file, truncated on startup.
- `--reset-db` deletes and rebuilds the database, requiring confirmation.

**Locations** follow `ttymap`'s convention via the `directories` crate v6 with brand `bakutrend`. On macOS that yields `~/Library/Application Support/bakutrend` for config and data, and `~/Library/Caches/bakutrend` for cache. `state_dir()` is Linux-only and falls back to `data_local_dir()`.

**Config** is TOML: source toggles (all 20, each with an `enabled` flag), poll interval, cluster threshold, score weights, local-filter keywords, retention window.

---

## 13. Error handling

`thiserror` 2 with one classified error enum per boundary — matching `ttymap`, which uses no `anyhow` anywhere. Variants are classified by their producing boundary so callers decide retry versus surface versus fail fast.

- A failed source is logged, backed off, and marked degraded. It never aborts a poll cycle or touches other sources.
- A malformed item is skipped and counted; a malformed feed does not abort the other nine.
- Network absence at startup is survivable: the TUI opens with whatever is in the database and reports zero sources reachable.
- `resolve()` returning `None` (no `$HOME`) degrades rather than panics.

---

## 14. Retention

- `items` — kept forever. About 1 500 items/day, roughly 500 K rows/year, trivial for SQLite.
- Stories are derived, so they need no retention policy at all.
- `view_samples` — pruned when older than 30 days, since a month-old sample informs no window.

Pruning runs once per poll cycle.

---

## 15. Testing strategy

TDD, red → green, vertical slices at five seams. Fixtures are real captured bytes, saved under `tests/fixtures/` — including the malformed cases from §5.4, because those are the cases that break.

1. **`source::rss::parse(bytes) -> Vec<ParsedItem>`** — one fixture per feed shape: the duplicate-guid feed, the missing-guid feed, the HTML-entity feed, the CDATA-in-escaped-text feed. Assert item counts, timestamps, and that identity keys stay distinct for every item in `qafqazinfo`.
2. **`source::telegram::parse(html) -> Vec<ParsedItem>`** — real captured preview pages. Assert post ids, datetimes, decoded view counts (`2.65K → 2650`), media-only posts, and posts missing views.
3. **`cluster::assign(existing, new)`** — same story in different words joins; two different stories sharing a common token stay apart; a syndicated repost joins its original.
4. **`score::rank(items, window, weights, now)`** — coverage counts **distinct outlets**, not sources; a cited repost contributes 0.5; an outlet with one cited and one uncited item contributes 1.0; per-channel normalization keeps a low-view channel competitive; freshness decays within the window; backfill items are absent from 1 h and 24 h. `now` is injected, so no test depends on wall-clock time.
5. **`store`** — upserting the same payload twice leaves coverage unchanged (the idempotency invariant); window queries exclude backfill correctly; `view_samples` pruning respects the 30-day boundary.

Existing tests are inline `#[cfg(test)] mod tests`, the dominant convention in `ttymap` (63 modules), with `use super::*;` first.

**What is deliberately not tested:** the exact ranking order of live data — it is not an invariant, it is an observation. The smoke test asserts the pipeline runs and renders, not that a particular story is number one.

**Smoke test.** Run the real binary against the real network. Verify the list populates with real stories, the windows switch, the detail pane shows per-outlet breakdown, and a dead source degrades without taking the app down.

---

## 16. Conventions adopted from `ttymap`

- Rust edition **2024**, `rust-version = "1.88"`.
- `ratatui = "0.30"`, `crossterm = "0.29"`, `unicode-width = "0.2"`, `log = "0.4"`.
- Blocking HTTP via `reqwest` with the `blocking` feature, as the engine does.
- `thiserror = "2"`; no `anyhow`.
- `directories = "6"` for path resolution, with the `state_dir()` fallback for macOS.
- `log` macros plus a hand-rolled opt-in `FileLogger`; no logging framework.
- CI gate: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, build and test.
- No `rustfmt.toml`; style is enforced by CI.

Deviations, stated deliberately: a single crate instead of a workspace (no plugin runtime, no IPC split), and `std::sync::mpsc` instead of `crossbeam-channel` (one consumer; stdlib suffices).

---

## 17. Dependencies

`ratatui`, `crossterm`, `unicode-width`, `reqwest` (blocking), `rusqlite` (bundled SQLite), `feed-rs`, `scraper`, `chrono`, `thiserror`, `log`, `directories`, `clap` (derive), `toml`, `serde`.

Each earns its place: `feed-rs` handles the four real-world malformation traps in §5.4 rather than spending the code on XML error recovery; `scraper` is needed because Telegram preview pages are HTML, not XML.

---

## 18. Known limits

- **Telegram history is 14 h.** Only `@meydantv` reaches further (~92 h). The 1-week window for engagement therefore depends on a locally running poller; until it has run for a week, the 7 d window is coverage-weighted and partly backfilled.
- **Views are biased by channel size.** Per-channel normalization mitigates this but does not eliminate it: it ranks relative buzz within a channel, not absolute reach across channels.
- **Syndication detection is lexical.** Citation phrasing varies; the regex will miss some reposts and will not misattribute originators, because it never tries to identify them.
- **The local filter is a keyword heuristic.** It will let some world news through when a local institution is mentioned in passing, and will drop some local news that uses unfamiliar place names.
- **Google News is used as a seed only,** one `when:7d` request on an empty database. Its terms permit personal feed-reader use; bakutrend does not depend on it for ongoing ranking.
- **Clustering is lexical.** Two outlets describing the same event with no shared vocabulary stay separate stories. This is the accepted trade-off of not using embeddings; if live data shows it failing badly, embeddings slot in behind the same `assign` interface.

---

## 19. Out of scope, deliberately

- Translation API integration.
- Embedding-based clustering.
- Non-interactive output mode.
- Category filtering UI (the section slug is stored, so it is a zero-migration addition later).
- Historical comparison beyond the immediately preceding window (e.g. "this week versus last week").
- Notifications, alerts, exports, web UI.
