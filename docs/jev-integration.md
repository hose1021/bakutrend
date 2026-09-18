# Integrating Jev (TypeSafe System One) into bakutrend

How to add a hosted judgment layer to this program without losing anything it already promises:
determinism, offline tests, the index, the movement column, and a build that runs on a machine
with no key.

Reference implementation of the same API, read for this document: **`jev-search`**
(`F:/Programming/jev-search`), files cited inline as `jev-search:<path>`.

Decision up front:

- **Phase 1 (recommended, do this first):** `bakutrend --ask "<plain-language request>"` reads
  the stories already in the database and asks Jev which of them answer the request. No schema
  change, no writes, no change to the index, ~200 lines including tests.
- **Phase 2 (optional):** Jev labels each ingested item once, at poll time, into a cache table.
  Adds a table, a pass in `poll_and_record`, and a bill that scales with volume.
- **Never:** Jev in the score, in the story key, in the grouping, or in the embeddings path.

---

## 1. What Jev is, and what it is not

Jev is TypeSafe's System One model behind one HTTP endpoint. It answers **typed questions about a
`state`** you supply. It does not write prose, does not search, and does not return embeddings.

```
POST https://api.typesafe.ai/v1/systemone
Authorization: Bearer <TYPESAFE_API_KEY>
Content-Type: application/json
```

```json
{
  "state": { "request": "what is trending in Baku this week", "stories": [ ... ] },
  "model": "jev-latest",
  "questions": {
    "s0": {
      "type": "noul",
      "instructions": "Is `stories[0]` about the subject the user asked for in `request`?",
      "criteria": {
        "true": "The headline or body discusses the same subject, even briefly or as one of several topics",
        "false": "The story is about something else that only shares words with the request, or is unrelated"
      }
    }
  }
}
```

```json
{
  "model": "jev-1.x",
  "answers": { "s0": { "type": "noul", "noul": 0.91 } },
  "usage": { "input_tokens": 812, "output_tokens": 34 }
}
```

Three question types (`https://docs.typesafe.ai/api`):

| `type` | `criteria` | Answer |
|---|---|---|
| `noul` | optional `{ "true": "...", "false": "..." }` | `{ noul: 0.0..1.0 }` — probability of yes |
| `choice` | required map `option -> rubric \| null` | `{ choice, probabilities, confidence }` |
| `score` | required array of level descriptions, ≥2 | `{ score, legend, probabilities, confidence }`, `score` may land between levels |

Rules that matter for implementation:

1. **The question key is never sent to the model.** It only labels the answer. The model reads
   `instructions`, `criteria`, and `state`.
2. **Instructions name the state keys in backticks** — `` `request` ``, `` `stories[0]` ``. That
   correspondence between state shape and instruction text is the entire prompt surface.
   `jev-search` relies on it in both judgments (`jev-search:src/lib/typesafe.ts:107` and `:213`).
3. **`criteria` is where the real specification lives.** A vague criteria pair produces a vague
   probability.
4. **All questions about one state go in one call.** `inferIntent` asks window + 12 source
   questions + 2 query questions in a single request (`jev-search:src/lib/typesafe.ts:107`); the
   reranker puts 40 item questions in one request (`:190`, `RERANK_BATCH`).
5. **Jev is a judge, not a generator.** It selects among options you construct. `jev-search`
   builds query candidates in code (`jev-search:src/lib/candidates.ts`) and lets Jev pick an
   index; `jev-ultrafast` writes text with a separate small LLM *only* for `TYPE_TEXT`. There is
   no "write me a search query" call.
6. **No answer means unknown, not no.** The API may omit a key. `jev-search` defaults a missing
   answer to `0` (`a?.type === 'noul' ? a.noul : 0`) — **do not copy that here.** See §7.

Error handling in the reference (`jev-search:src/lib/typesafe.ts:67`): `5xx` → "temporarily
unavailable", `429` → "too many requests", anything else → `HTTP <status>`. The response body is
discarded on purpose — it may echo the request, which in this program is news text.

---

## 2. How jev-search uses it

Two judgments, both over a state that code assembled.

**Judgment 1 — `inferIntent`.** State is `{ request, now, candidates }`. Questions: one `choice`
for the time window (options described by `WINDOWS` in code), one `noul` per source ("Would
Hacker News threads fit this request?" with yes/no rubrics from `SOURCES`), one `choice` over
the candidate queries, one `choice` for the entity name. Sources above `0.6` are searched
(`SOURCE_PROB_THRESHOLD`); if none clears it, the default set is used rather than searching
nothing.

**Judgment 2 — `rerank`.** Runs per engine lane, in batches of 40. State is
`{ request, results: [{source, title, snippet}] }`, one `noul` per row:

> Is `results[i]` about the subject the user asked for in `request`?

The result is a **probability of topicality**, used as an ordering key, never as a filter and
never as a quality claim. `compareItems` rounds it to whole percent before comparing
(`jev-search:src/lib/rank.ts:41`) — the code already treats the probability as coarse.

Failure isolation: a rerank failure sets the lane's `error` and leaves rows `ranked: false`;
`compareItems` sorts unranked rows to the bottom. Jev failing degrades the *order*, never the
*result set*.

Caching: `cachedSearch` caches engine responses only (`jev-search:src/lib/cache.ts:33`).
Judgments are **not** cached — every request re-asks. That is affordable there because repeats
are user-driven; a poller's repeats are scheduled. See §6.

---

## 3. Mapping jev-search onto bakutrend

| jev-search | bakutrend | Note |
|---|---|---|
| `SOURCES` + `ask` rubrics | the configured `sources` list, already polled | no counterpart needed; the poller already decided what to read |
| `buildCandidates` + query `choice` | **no counterpart** | bakutrend does not build search queries; it already holds the text |
| window `choice` | `Window::{Hour, Day, Week}` + their labels | direct port, reuse `Window::label()` |
| `rerank` over engine rows | `noul` per stored story, in batches | the direct port; phase 1 |
| engine lanes + `found`/`lane` events | **no counterpart** | there is no request-time pipeline; rendering is local and synchronous |
| `cachedSearch` (KV, TTL by window) | `items`/`view_samples` are already durable | nothing to add for phase 1 |
| per-engine 15 s / overall 30 s deadline | per-call 20 s deadline | one call, one timeout |
| `EmbeddingProvider` (this repo) | `EmbeddingProvider` (`src/embed.rs:15`) | **not the same thing.** Jev returns no vectors, so `embeddings`/`semantic_threshold` cannot be filled by Jev |
| `NullProvider` (`src/embed.rs:128`) | `NullJudge` | the degradation shape to copy |

Two honest differences to keep:

- `jev-search` may safely default a missing answer to `0`. Here a missing answer is
  `unjudged`, and the screen must say so, exactly as a missing `Velocity` sample is not a zero
  rate.
- `jev-search` fires a speculative Google search alongside the judge to hide latency
  (`jev-search:src/lib/pipeline.ts:119`). There is nothing to speculate here: ranking a window
  is a local SQLite read over milliseconds.

---

## 4. Phase 1 — `bakutrend --ask "<request>"`

A new print mode beside `--poll-only` and `--export`. It reads, never writes: `Store::open_read_only`,
the same connection the web server uses (`src/web.rs:100`).

Flow:

1. Ask Jev which window the request wants (`choice` over `1h`/`24h`/`7d`).
2. Rank that window with the existing `app::rank_window` — the prominence index decides which
   stories are worth judging.
3. Take the top `ASK_CANDIDATES` (48) and ask Jev, in batches of `STORIES_PER_CALL` (12), which
   are about the request (`noul`).
4. Print the survivors ordered by relevance, keeping the prominence index beside each row.

No index, grouping, key, schema or `ALGORITHM_VERSION` change. `make ci` stays offline.

### 4.1 `src/error.rs` — one more boundary

```rust
#[derive(Debug, thiserror::Error)]
pub enum JevError {
    #[error("TYPESAFE_API_KEY is not set")]
    NoKey,
    #[error("Jev is temporarily unavailable")]
    Unavailable,
    #[error("Jev is receiving too many requests")]
    RateLimited,
    #[error("Jev could not process this request (HTTP {status})")]
    Status { status: u16 },
    #[error("network error calling Jev: {source}")]
    Network {
        #[source]
        source: reqwest::Error,
    },
    /// A question came back unanswered. Never read as a zero: see `src/jev.rs`.
    #[error("Jev answered {got} of {want} questions at {batch}")]
    Incomplete { got: usize, want: usize, batch: String },
}

/// `ask` crosses two boundaries — SQLite and the network — so it needs both failures in one
/// type, the way `EmbedJobError` does for the embedding backfill.
#[derive(Debug, thiserror::Error)]
pub enum AskError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("jev: {0}")]
    Jev(#[from] JevError),
}
```

The status mapping is copied from `jev-search:src/lib/typesafe.ts:67`, including the decision to
not surface the provider's response body.

### 4.2 `src/jev.rs` — the client, the questions, the trait

```rust
//! Jev (TypeSafe System One) judgments. Optional: nothing here is fatal, and a build with no key
//! behaves exactly like the shipped one.
//!
//! Docs: https://docs.typesafe.ai/api. One POST carries a `state` and a map of typed questions;
//! the answers come back under the same keys. The key is not sent to the model, so the state keys
//! and the backticked names inside `instructions` are the whole contract between code and model.

use std::collections::BTreeMap;
use std::io::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app::rank_window;
use crate::config::Config;
use crate::error::{AskError, JevError};
use crate::store::{Store, Window};

pub const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub const DEFAULT_MODEL: &str = "jev-latest";
/// One call, one deadline. `jev-search` allows a lane 15 s inside a 30 s request.
const TIMEOUT_SECS: u64 = 20;
/// Stories judged per call. The state grows with the batch, so this is a context and bill limit,
/// not only a round-trip limit.
pub const STORIES_PER_CALL: usize = 12;
/// Stories carried into judging. The index already ordered them; judging a week window wholesale
/// would be a thousand stories and a bill to match.
pub const ASK_CANDIDATES: usize = 48;
/// Below this the story is not an answer to the request. Used to separate what the reader asked
/// for from what merely shares a word with it.
const RELEVANCE_FLOOR: f64 = 0.5;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// Yes/no, answered as a probability.
    Noul {
        instructions: String,
        criteria: NoulCriteria,
    },
    /// One option from a set this program defines.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, Option<String>>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct Reply {
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Anything that can answer typed questions. Tests supply a fixture-backed implementation, so
/// every test in this file runs offline and needs no key — the reason `Fetcher` exists in
/// `src/source/http.rs`.
pub trait Judge: Send + Sync {
    /// Names the model in every stored or printed judgment, and is the cache key in phase 2.
    fn model(&self) -> &str;
    fn ask(&self, state: &Value, questions: &BTreeMap<String, Question>) -> Result<Reply, JevError>;
}

/// No judgments at all, and never a silent yes. `--export` runs on a runner with no key and uses
/// this: the snapshot is the ranking it always was.
pub struct NullJudge;

impl Judge for NullJudge {
    fn model(&self) -> &str {
        "null"
    }

    fn ask(&self, _state: &Value, _questions: &BTreeMap<String, Question>) -> Result<Reply, JevError> {
        Err(JevError::NoKey)
    }
}

pub struct JevClient {
    api_key: String,
    model: String,
    client: reqwest::blocking::Client,
}

impl JevClient {
    /// `None` when no key is configured. Absence is a normal state, not a failure: nothing else
    /// in this program needs a key.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("TYPESAFE_API_KEY").ok()?;
        if api_key.trim().is_empty() {
            return None;
        }
        let model = std::env::var("TYPESAFE_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .build()
            .ok()?;
        Some(Self { api_key, model, client })
    }

    /// The request body, built on its own so a test can assert its shape without a network.
    pub fn body(state: &Value, model: &str, questions: &BTreeMap<String, Question>) -> Value {
        json!({ "state": state, "model": model, "questions": questions })
    }

    /// Answers out of a response body, or `Err` when the body is not a Reply. Kept separate from
    /// the call so a captured response can be tested byte for byte.
    pub fn parse(body: &str) -> Result<Reply, JevError> {
        serde_json::from_str(body).map_err(|_| JevError::Unavailable)
    }
}

impl Judge for JevClient {
    fn model(&self) -> &str {
        &self.model
    }

    fn ask(&self, state: &Value, questions: &BTreeMap<String, Question>) -> Result<Reply, JevError> {
        let response = self
            .client
            .post(ENDPOINT)
            .bearer_auth(&self.api_key)
            .json(&Self::body(state, &self.model, questions))
            .send()
            .map_err(|source| JevError::Network { source })?;
        match response.status().as_u16() {
            200 => {
                let text = response.text().map_err(|source| JevError::Network { source })?;
                Self::parse(&text)
            }
            // The provider's body is not read: it may echo the news text sent as state.
            429 => Err(JevError::RateLimited),
            status if status >= 500 => Err(JevError::Unavailable),
            status => Err(JevError::Status { status }),
        }
    }
}
```

### 4.3 `src/jev.rs` — the questions

This is the prompt library. Every string below is the specification; keep them together so a
change is visible in one diff.

```rust
/// The window the request wants. Rubrics restate `Window`'s own meaning rather than inventing a
/// second one.
pub fn window_question() -> Question {
    let mut criteria = BTreeMap::new();
    criteria.insert("1h".to_string(), Some("Only what happened in the last hour, or the request asks for the very latest news".to_string()));
    criteria.insert("24h".to_string(), Some("Today or the last day, or the request asks what is current".to_string()));
    criteria.insert("7d".to_string(), Some("The last week, or the request asks for trends, developments or what has been happening lately".to_string()));
    Question::Choice {
        instructions:
            "Does `request` ask for news from a particular period, and if so which? Judge only from what the request says or clearly implies; state says what the request is and what day it is today. A request about a general subject with no time cue wants the last week.".to_string(),
        criteria,
    }
}

/// One story, judged for topicality. Wording follows `rerank` in jev-search; the `true`/`false`
/// pair is what keeps "shares a word" out of the answer.
pub fn story_question(index: usize) -> Question {
    Question::Noul {
        instructions: format!(
            "Is `stories[{index}]` about the subject the user asked for in `request`?"
        ),
        criteria: NoulCriteria {
            yes: "The headline or body discusses the same subject the user asked about, even briefly or as one of several topics".to_string(),
            no: "The story is about something else that only shares words with the request — a different meaning of the same word, a different person or company with the same name — or is unrelated".to_string(),
        },
    }
}

/// The state a story batch is judged against. Titles are kept as their outlet wrote them;
/// Azeri and Russian text is not translated, and the judge is not asked to translate it.
pub fn story_state(request: &str, now: i64, stories: &[StoryText]) -> Value {
    let day = chrono::DateTime::from_timestamp(now, 0)
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default();
    json!({
        "request": request,
        "now": day,
        "stories": stories.iter().map(|s| json!({
            "outlets": s.outlets,
            "title": s.title,
            "description": s.description,
        })).collect::<Vec<_>>(),
    })
}

/// What a story looks like to the judge: the fields a reader has, and nothing else.
pub struct StoryText {
    pub title: String,
    pub description: String,
    pub outlets: Vec<String>,
}
```

### 4.4 `src/jev.rs` — the run

```rust
/// What one run cost and what it judged, so the caller can print the evidence.
#[derive(Debug)]
pub struct AskReport {
    pub window: Window,
    pub judged: usize,
    pub answered: usize,
    pub unanswered: usize,
    pub tokens: u64,
    pub model: String,
    pub hits: Vec<Hit>,
}

impl Default for AskReport {
    fn default() -> Self {
        // `Window` has no default of its own; the week window is the one a request with no time
        // cue is judged to want, so it is the fallback here too.
        Self {
            window: Window::Week,
            judged: 0,
            answered: 0,
            unanswered: 0,
            tokens: 0,
            model: String::new(),
            hits: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct Hit {
    pub story: crate::score::ScoredStory,
    pub relevance: f64,
}

/// Which stored stories answer a plain-language request.
///
/// The window is asked for first, then that window is ranked with the one ranking implementation
/// the program has, and only its head is judged. Order of operations matters: judging before
/// ranking would mean judging stories nobody would ever be shown.
pub fn ask(
    store: &Store,
    config: &Config,
    judge: &dyn Judge,
    request: &str,
    now: i64,
) -> Result<AskReport, AskError> {
    let mut report = AskReport { model: judge.model().to_string(), ..Default::default() };

    let mut window_questions = BTreeMap::new();
    window_questions.insert("window".to_string(), window_question());
    let window_state = json!({
        "request": request,
        "now": chrono::DateTime::from_timestamp(now, 0)
            .map(|t| t.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
    });
    let reply = judge.ask(&window_state, &window_questions)?;
    report.tokens += reply.usage.input_tokens + reply.usage.output_tokens;
    report.window = match reply.answers.get("window") {
        Some(Answer::Choice { choice, .. }) => parse_window(choice).unwrap_or(Window::Week),
        _ => Window::Week,
    };

    let stories = rank_window(store, config, report.window, now).unwrap_or_default();
    report.judged = stories.len().min(ASK_CANDIDATES);
    let candidates = &stories[..report.judged];

    let mut scored: Vec<(usize, f64, bool)> = Vec::with_capacity(candidates.len());
    for (batch_index, batch) in candidates.chunks(STORIES_PER_CALL).enumerate() {
        let texts: Vec<StoryText> = batch
            .iter()
            .map(|story| StoryText {
                title: story.title.clone(),
                description: story.description.clone().unwrap_or_default(),
                outlets: story.outlets.iter().map(|o| o.outlet.clone()).collect(),
            })
            .collect();
        let mut questions = BTreeMap::new();
        for index in 0..texts.len() {
            questions.insert(format!("s{index}"), story_question(index));
        }
        let reply = judge.ask(&story_state(request, now, &texts), &questions)?;
        report.tokens += reply.usage.input_tokens + reply.usage.output_tokens;
        for (index, _) in texts.iter().enumerate() {
            let key = format!("s{index}");
            // A question with no answer is `unjudged`, never relevance zero. A zero would sink
            // the story into the same place as a story the judge actively rejected, and this
            // program does not present a missing observation as a measurement.
            match reply.answers.get(&key) {
                Some(Answer::Noul { noul }) => {
                    report.answered += 1;
                    scored.push((batch_index * STORIES_PER_CALL + index, *noul, true));
                }
                _ => {
                    report.unanswered += 1;
                    scored.push((batch_index * STORIES_PER_CALL + index, 0.0, false));
                }
            }
        }
    }

    let mut hits: Vec<Hit> = scored
        .into_iter()
        .filter(|(_, relevance, judged)| *judged && *relevance >= RELEVANCE_FLOOR)
        .map(|(index, relevance, _)| Hit { story: candidates[index].clone(), relevance })
        .collect();
    hits.sort_by(|a, b| {
        b.relevance
            .partial_cmp(&a.relevance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    report.hits = hits;
    Ok(report)
}

fn parse_window(choice: &str) -> Option<Window> {
    Window::all().into_iter().find(|w| w.label() == choice)
}
```

### 4.5 Printing the result

```rust
/// The list, with the evidence beside it: a judgment shown without its model and its window is
/// an opinion of unknown origin.
pub fn render(report: &AskReport, request: &str, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "{request}")?;
    writeln!(
        out,
        "{} · {} stories judged · {} answered · {} unanswered · {} tokens · {}",
        report.window.label(),
        report.judged,
        report.answered,
        report.unanswered,
        report.tokens,
        report.model,
    )?;
    if report.hits.is_empty() {
        writeln!(out, "no stored story answers this request in that window")?;
        return Ok(());
    }
    writeln!(out)?;
    for (rank, hit) in report.hits.iter().enumerate() {
        writeln!(
            out,
            "{:>2}. {:>3}% relevance · {:.3} index · {} outlets · {}",
            rank + 1,
            (hit.relevance * 100.0).round() as i64,
            hit.story.score,
            hit.story.spread,
            hit.story.title,
        )?;
    }
    writeln!(out)?;
    writeln!(out, "relevance is a Jev judgment about the request, not a measure of quality or reach")?;
    if report.unanswered > 0 {
        writeln!(out, "{} stories were not judged and are not in this list", report.unanswered)?;
    }
    Ok(())
}
```

### 4.6 Wiring

- `src/cli.rs`: one field.
  ```rust
  /// Judge which stored stories answer a plain-language request. Needs TYPESAFE_API_KEY.
  #[arg(long, value_name = "REQUEST")]
  pub ask: Option<String>,
  ```
- `src/main.rs`: dispatch before the poll paths, and open read-only.
  ```rust
  if let Some(request) = cli.ask.as_deref() {
      let Some(judge) = JevClient::from_env() else {
          eprintln!("TYPESAFE_API_KEY is not set; --ask needs it. Everything else runs without it.");
          std::process::exit(2);
      };
      let store = Store::open_read_only(&db_path)?;
      let report = jev::ask(&store, &config, &judge, request, chrono::Utc::now().timestamp())?;
      jev::render(&report, request, &mut std::io::stdout())?;
      return Ok(());
  }
  ```
- `src/lib.rs`: `pub mod jev;`
- `Cargo.toml`: `reqwest` gains the `json` feature
  (`features = ["blocking", "json"]`). The client sends a JSON body, and `reqwest` does not
  enable that feature by default; without it `.json(...)` does not compile. The alternative is
  `Content-Type: application/json` plus a `serde_json::to_string` body, which is one line more
  for no gain.
- Nothing else. `--export`, `--serve`, `--poll-only` and the TUI are untouched, and a machine
  with no key is unchanged.

### 4.7 Tests (`#[cfg(test)] mod tests` in `src/jev.rs`)

A `FixtureJudge` implementing `Judge` — no network, no key, deterministic. Assertions that fail
on a plausible bug:

1. **Question shape.** `JevClient::body(...)` serialises `noul` with `criteria.true`/`.false`
   and `choice` criteria as `option -> rubric|null`. Fails if the serde tagging is wrong, which
   is the difference between a working call and a `400`.
2. **Instruction names the state.** `story_question(3)`'s instruction contains `` `stories[3]` ``,
   matching the index the state uses. Fails if a batch is offset by one, which would silently
   score every story against its neighbour.
3. **Missing answer is not zero.** A fixture that omits `s4` yields `unanswered == 1` and no hit
   for that story, rather than a `0.0` relevance row.
4. **Floor and ordering.** Relevance `0.9`, `0.4`, `0.6` yields two hits, best first.
5. **Window parsing.** `"7d"` → `Window::Week`; an unknown choice falls back to the week rather
   than panicking.
6. **Batch boundary.** 13 candidates produce two calls of 12 and 1, and every answer maps back
   to the right story.
7. **`NullJudge` degrades.** No key answers `Err(NoKey)` and never a default yes.
8. **Cost is counted.** `tokens` equals the sum of the fixture's `usage` across calls.

---

## 5. Phase 2 — poll-time labels (optional)

Only if the labels are wanted on the TUI, the web page and the snapshot. This adds storage and a
recurring bill; phase 1 adds neither.

**What it gives:** per-item `language` (`choice`), `about_azerbaijan` (`noul`), `category`
(`choice`), `substance` (`score`) — shown on the card and usable as an extra sort.

**What it must not do:** enter the score, the story key, the grouping, or `ALGORITHM_VERSION`.
The label is a new fact beside the index, never a term in it. If it entered the score, every
stored ranking would stop being comparable to every other and the movement column would report
invented change.

Schema, in `SCHEMA` (`CREATE TABLE IF NOT EXISTS` is re-run on every open, so an existing
database gains the table):

```sql
-- Judgments about one item, bought once and kept. Keyed by model and prompt version because a
-- second model's answers are not comparable with the first's, and a reworded prompt is a
-- different measurement of the same item.
CREATE TABLE IF NOT EXISTS jev_labels (
  item_id        INTEGER NOT NULL REFERENCES items(id),
  model          TEXT NOT NULL,
  prompt_version INTEGER NOT NULL,
  answer         TEXT NOT NULL,
  created_at     INTEGER NOT NULL,
  PRIMARY KEY (item_id, model, prompt_version)
);
```

Store methods, mirroring the embedding cache exactly (`store.rs:552`-`640`):

- `current_label_model() -> Option<String>` — newest written model.
- `labels_for_range(from, to, model) -> HashMap<i64, Label>`.
- `save_labels(model, prompt_version, rows, now)`.
- `items_missing_labels(model, prompt_version, limit)` — newest first, `LIMIT`-bounded, exactly
  like `items_missing_embeddings`, for the same reason: a year-old database must not try to label
  its whole backlog in one cycle.

Job, mirroring `embed::embed_pending` (`src/embed.rs:149`):

```rust
/// Label the newest unlabeled items, up to `limit`. Runs after a poll, on the writer connection,
/// and never inside a ranking pass: a refresh must not wait on a provider, and no item may be
/// judged twice.
pub fn label_pending(store: &mut Store, judge: &dyn Judge, limit: usize, now: i64) -> Result<usize, JevError>
```

Call it from `poll_and_record` (`src/poller.rs:151`) after the ranking is recorded. That
function gains one parameter, `judge: &dyn Judge` — four call sites in `src/main.rs` and the two
in `tests/pipeline.rs`, each passing `&judge` in production and `&NullJudge` in tests. Putting
the step in `poll_and_record` rather than in each caller is what keeps `--poll-only`, the TUI,
`--serve` and `--export` from drifting apart.

Config, one table, off by default:

```toml
[jev]
enabled          = false   # a build with no key is inert whatever this says
labels_per_cycle = 8       # bound on paid calls per cycle
```

`enabled = false` plus no key means `NullJudge`: no table rows, no calls, no change to any
output. The GitHub Pages workflow (`--export`, every thirty minutes, on a public runner with no
secrets) must keep passing through that path untouched.

---

## 6. Cost, keys, and failure

- **Keys live in the environment only** — `TYPESAFE_API_KEY`, optional `TYPESAFE_MODEL`. Not in
  `config.toml`: that file is user-editable, commonly dotfile-synced, and read on every run,
  including the public runner's.
- **Inert without a key.** Missing key = `NullJudge` = shipped behaviour. Never an error at
  startup, never a half-labelled screen.
- **Bounded per run.** Phase 1: `ASK_CANDIDATES` (48) stories, i.e. at most 5 calls. Phase 2:
  `labels_per_cycle` per cycle, newest first.
- **Bounded in time.** One 20 s timeout per call; a `--ask` run cannot hang past that.
- **`429` and `5xx` never damage the database.** Phase 1 prints the error and exits nonzero.
  Phase 2 logs and ends the pass; the poll cycle is unaffected.
- **A missing answer is unknown.** `unanswered` is counted and printed. It is never a zero, never
  a default, and never silently dropped from a total.
- **State is news text.** It leaves the machine. Say so in the README's data section the way
  `jev-search` says search text goes to TypeSafe and Search1API. Do not send the whole stored
  body: the judge needs a headline, a lede and the outlets, not 3000 characters of one feed's
  copy of a wire story.
- **Bodies are never surfaced.** A provider response body may echo the state; the client reads
  status codes only.

---

## 7. Order of work, and the prompts

Each step is one commit that keeps `make ci` green.

**P0 — recon (no code). Done; see `SPEC.md` §Reference corrections.**
> Read `src/jev.rs`'s shape here, then in the repository read `src/lib/typesafe.ts:67`, `:107`,
> `:213` and `src/lib/rank.ts:41` of jev-search at `F:/Programming/jev-search`. Report the exact
> request body `inferIntent` sends, how many questions one call carries, and how a missing answer
> is treated. Change nothing.

**P1 — client and trait.**
> Add `JevError` and `AskError` to `src/error.rs`, and `src/jev.rs` with `Question`, `Answer`,
> `Reply`, `Usage`, the `Judge` trait, `NullJudge`, `JevClient::from_env`, `JevClient::body`,
> `JevClient::parse`, `ask` and `render`, exactly as specified in `docs/jev-integration.md`.
> Add `pub mod jev;` to `src/lib.rs`. Do not touch `src/score.rs`, `src/cluster.rs`,
> `src/store.rs`, `ALGORITHM_VERSION`, or any existing test. Add the eight tests in §4.7; they
> must run offline and never need a key. Stop when `make ci` passes.

**P2 — the CLI mode.**
> Add `--ask <REQUEST>` to `src/cli.rs` and the dispatch arm to `src/main.rs`, opening the store
> read-only. `--serve`, `--export`, `--poll-only` and the TUI must behave exactly as before. Stop
> when `make ci` passes and `cargo run -- --ask "..."` without `TYPESAFE_API_KEY` prints the
> missing-key message and exits 2.

**P3 — documentation.**
> Amend `docs/superpowers/specs/2026-09-14-bakutrend-design.md` §4 (the "no hosted AI service"
> non-goal) and §19 ("a concrete embedding provider" is out of scope) to say what is now in
> scope, that it is opt-in, and that it never enters the score. Add a `--ask` section to
> `README.md`, list `TYPESAFE_API_KEY` where the other operational facts are, and add the news
> text that leaves the machine to the data section. State plainly that Jev returns no vectors and
> that `semantic_threshold` is still inert.

**P4 — phase 2, only if asked for.**
> Add the `jev_labels` table, the four store methods, `jev::label_pending`, the `[jev]` config
> table with its validation, the extra parameter on `poll_and_record` and its four call sites,
> and the `FakeJudge`-driven tests. Prove with `make ci` that a build with `enabled = false` or no
> key writes no rows and changes no output.

**P5 — verification.**
> Run `make ci`. Then run one real `--ask` against a real database with a real key and report:
> the window chosen, how many stories were judged, how many answered, the token count, and the
> first three rows. Report a `429` or a `5xx` if it happens; do not retry in a loop.

---

## 8. What must not change

| Thing | Why it must not take a Jev judgment |
|---|---|
| `score::rank` and `Weights` | the score is the weighted sum of its four parts, asserted in tests; a fifth term makes every stored ranking incomparable and `ALGORITHM_VERSION` meaningless |
| `score::ALGORITHM_VERSION` | movement compares stored rankings; a bump declared for a number that did not change is a lie, and a number changed without a bump is a wrong answer |
| `cluster::signature` / `Group::key` | I1: a key must not depend on the active window or on a second call to a nondeterministic service, or the movement column reports movement no reader caused |
| `embed::EmbeddingProvider` | Jev returns no vectors; `semantic_threshold` stays inert |
| `Store::open` on the writer | only the poller writes; `--serve` and `--export` stay read-only |
| `Fetcher` | the seam that makes the whole poll cycle testable offline; a `Judge` seam is the same idea, not a replacement |

## 9. Verification

```sh
make ci                                                       # fmt, clippy -D warnings, tests
cargo run -- --ask "what is trending in Baku this week"        # without a key: exit 2, one message
TYPESAFE_API_KEY=... cargo run -- --ask "..."                  # real run, real bill
cargo run -- --export /tmp/site && ls /tmp/site                # snapshot unchanged, no key needed
```

A green `make ci` on a machine with no `TYPESAFE_API_KEY` is the proof that the whole thing is
optional. A `--ask` run that names its window, its model, its judged/answered/unanswered counts
and its token cost is the proof that nothing is presented as a measurement it is not.

## 10. Rollback

Delete `src/jev.rs`, the `jev` module line, the `--ask` field and arm, and the two error enums.
Phase 1 writes nothing, so no database changes hands. Phase 2 leaves one unused table, which is
the same shape of residue `embeddings` already is, and no migration is needed to remove it.
