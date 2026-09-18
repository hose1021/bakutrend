# Spec: `bakutrend --ask` — a Jev judgment beside the index

Status: awaiting approval. No code is written until this spec is approved.

- Design of record for this change: `docs/jev-integration.md` (phase 1, §4).
- Design of record for the program: `docs/superpowers/specs/2026-09-14-bakutrend-design.md`.
- Scope of this spec: **phase 1 only**. Phase 2 (`jev-labels`) is recorded in the capability map as deferred, not specified here.

## Capability map

| Module id | Responsibility | Depends on |
|---|---|---|
| `jev-client` | Typed questions and answers, the `Judge` seam, the Jev HTTP client, one `ask` run, its report and its printed form | — |
| `ask-cli` | The `--ask <REQUEST>` flag, dispatch before the poll paths, the read-only store open, the missing-key exit | `jev-client` |
| `jev-labels` | Poll-time per-item labels in a `jev_labels` table (phase 2). **Deferred by decision; recorded only.** | `jev-client` |

Dependency direction: `ask-cli → jev-client`, `jev-labels → jev-client`. One way, no cycles.

Build order: `jev-client` → `ask-cli` → documentation. Each step is one commit that keeps `make ci` green.

## Objective

Add one read-only print mode: `bakutrend --ask "<plain-language request>"` reads the stories already stored, asks Jev (TypeSafe System One) which of them answer the request, and prints the survivors with the prominence index beside each one.

Why: the index answers "what is Azerbaijan reading", not "what answers my question". Topicality is a second fact about a story, so it sits beside the index and never inside it. The score, the story key, the grouping and the embeddings path do not change, and every ranking already stored stays comparable.

Who uses it: an operator at a terminal who has `TYPESAFE_API_KEY` set. A machine with no key keeps today's behaviour. The TUI, `--serve`, `--export` and `--poll-only` run exactly as they do now, and `make ci` never needs a key or a network.

Success is defined in **Success criteria**.

## Tech stack

- Rust, edition 2024, MSRV 1.88 (already in `Cargo.toml`).
- No new crates. `reqwest` is already a dependency with the `blocking` feature; it gains the `json` feature because the client posts a JSON body. This is the one manifest change and it is in **Ask first**.
- Already-installed libraries carry the rest: `serde_json` and `serde` for the wire types, `thiserror` for the error enums, `chrono` for timestamps, `rusqlite` through the existing `Store`.
- Endpoint `https://api.typesafe.ai/v1/systemone`; default model `jev-latest`; key from `TYPESAFE_API_KEY`; optional `TYPESAFE_MODEL` override. Keys live in the environment only.

## Commands

```sh
make build        # cargo build
make test         # cargo test — offline, no key
make lint         # cargo fmt --all --check && cargo clippy --all-targets -- -D warnings
make ci           # lint, then test — the gate every commit passes, with no key set
make run          # the TUI

cargo run -- --ask "what is trending in Baku this week"    # no key: one message, exit 2
TYPESAFE_API_KEY=... cargo run -- --ask "..."              # real run, real bill
cargo run -- --export /tmp/site                            # snapshot path, stays key-free
cargo run -- --sort ... # not part of this change
```

## Project structure

```
src/jev.rs         new: Question/Answer/Reply/Usage, the Judge trait, NullJudge, JevClient,
                   ask(), AskReport, render(), and the offline tests
src/error.rs       add JevError and AskError, following the one-enum-per-boundary convention
src/cli.rs         add the --ask field
src/main.rs        add the dispatch arm: read-only store, exit 2 when the key is absent
src/lib.rs         add pub mod jev;
Cargo.toml         reqwest features = ["blocking", "json"]
README.md          --ask section, TYPESAFE_API_KEY, and the data that leaves the machine
docs/jev-integration.md                               design of record for this change
docs/superpowers/specs/2026-09-14-bakutrend-design.md  §4 and §19 amended by the docs step
```

Not touched: `src/score.rs`, `src/cluster.rs`, `src/store.rs`, `src/embed.rs`, `src/poller.rs`, `src/config.rs`, `src/ui.rs`, `src/web.rs`, `src/export.rs`, `src/i18n.rs`, `src/text.rs`, `src/source/**`, `tests/**`.

## Code style

One real snippet from this repository sets the tone: module doc first, then a public trait whose doc says why it exists, not what it does.

```rust
//! Blocking HTTP fetching. The poller is sequential, so a client pool buys nothing;
//! a single client with a browser User-Agent is enough (`azertag.az` returns 400 without one).

/// Anything that can turn a source row into parsed items. Tests supply a fixture-backed
/// implementation so the whole poll cycle runs offline.
pub trait Fetcher {
    fn fetch(&self, source: &SourceRow) -> Result<ParseOutcome, FetchError>;
}
```

Rules:

- `cargo fmt --all` defaults; `clippy -D warnings` — a warning fails the build.
- One `thiserror` enum per boundary in `src/error.rs`. `AskError` wraps `StoreError` and `JevError` because `ask` crosses SQLite and the network.
- No `unwrap()` or `expect()` outside tests, and no panic on a path a user can reach.
- Constants are `SCREAMING_SNAKE` with a doc comment stating the unit and why that value: `TIMEOUT_SECS`, `STORIES_PER_CALL`, `ASK_CANDIDATES`, `RELEVANCE_FLOOR`, `DEFAULT_MODEL`.
- Comments are English, ASD-STE100: short sentences, one idea, the reason rather than the mechanics. No commented-out code and no `TODO` placeholders.
- The degradation shape is copied from the existing seam:
  ```rust
  /// No judgments at all, and never a silent yes. `--export` runs on a runner with no key and
  /// uses this: the snapshot is the ranking it always was.
  pub struct NullJudge;
  ```
- Deletion over addition: no trait method, constant, flag or config key that no phase-1 path uses.

## Testing strategy

- Framework: the standard harness, `cargo test`. No new dev-dependency.
- Unit tests live inline in `#[cfg(test)] mod tests` at the bottom of `src/jev.rs`, as in `src/cli.rs` and `src/embed.rs`.
- The seam is a `FixtureJudge` implementing `Judge` and returning canned `Reply` values. Every test in `src/jev.rs` runs offline, deterministically, with no key and no network — the same reason `Fetcher` exists.
- `tests/pipeline.rs` is not touched: this change adds no poller step.
- Required assertions (design of record §4.7, plus one addition):

1. **Question shape.** `JevClient::body` serialises `noul` with `criteria.true`/`.false` and `choice` criteria as `option -> rubric|null`. Fails if the serde tagging is wrong, which is a `400` against the live API.
2. **Instruction names the state.** `story_question(3)`'s instruction contains `` `stories[3]` ``, the index the state uses.
3. **Missing answer is not zero.** A fixture that omits `s4` yields `unanswered == 1` and no hit for that story.
4. **Floor and ordering.** Relevance `0.9`, `0.4`, `0.6` yields two hits, best first.
5. **Window parsing.** `"7d"` → `Window::Week`; an unknown choice falls back to the week rather than panicking.
6. **Batch boundary.** 13 candidates produce two calls of 12 and 1, and every answer maps back to its own story.
7. **`NullJudge` degrades.** It answers `Err(NoKey)` and never a default yes.
8. **Cost is counted.** `tokens` equals the sum of the fixture's `usage` across calls.
9. **Evidence is printed** (addition to §4.7). `render` writes the window, the model, the judged/answered/unanswered counts and the token cost, and it names the unanswered stories when there are any. A judgment printed without its origin is an opinion of unknown provenance.

- Not covered by tests: the live endpoint. It needs a key and a bill, so the manual verification run covers it.

## Boundaries

**Always**

- Keep `make ci` green on a machine with no `TYPESAFE_API_KEY`.
- Open the store read-only for `--ask`, the same connection the web server uses.
- Count unanswered questions and print them; never present a missing answer as a measurement.
- Print the model, the window, the counts and the token cost with every judgment.
- Keep the question instructions and criteria together in one block in `src/jev.rs`, so a prompt change is visible in one diff.
- Write code, comments and repository files in English.

**Ask first**

- Enabling the `json` feature on `reqwest` — the only manifest change.
- Changing `ASK_CANDIDATES` (48), `STORIES_PER_CALL` (12), `TIMEOUT_SECS` (20), `RELEVANCE_FLOOR` (0.5) or `DEFAULT_MODEL`: these are calibration values, and the first real run may justify new ones.
- Rewording any instruction or criteria string. The prompt is the specification of the judgment.
- Anything in phase 2 (`jev-labels`), including its table, its config table and its poller parameter.
- Amending the program design document outside its §4 and §19.

**Never**

- Put a Jev judgment in `score::rank`, in `Weights`, or as a fifth score term.
- Bump, repurpose or work around `score::ALGORITHM_VERSION` (it stays 2).
- Put a judgment in `cluster::signature` or `Group::key`, in the grouping, or in the embeddings path. Jev returns no vectors, so `semantic_threshold` stays inert.
- Write to the database from `--ask`, change the schema, or open the writer connection.
- Default a missing answer to `0.0`, drop it from the counts, or sort it with the stories the judge rejected.
- Send the whole stored body. The judge gets a title, a lede and the outlet names.
- Surface a provider response body: it may echo the news text sent as state.
- Put the API key in `config.toml`, or require a key for any mode other than `--ask`.
- Add a second ranking implementation: `app::rank_window` is the one ranking the program has.
- Retry in a loop on `429` or `5xx`.

## Success criteria

1. `make ci` passes with no `TYPESAFE_API_KEY` set, on a machine with no network access to TypeSafe.
2. `cargo run -- --ask "..."` with no key prints one message naming the missing key and exits `2`, and writes nothing.
3. A run with a key prints: the window label (`1h`, `24h` or `7d`), the model, `judged`, `answered`, `unanswered`, the token count, and the hits ordered by relevance, each row showing its relevance percentage, its index score, its spread and its title.
4. A fixture run whose reply omits one answer counts it in `unanswered` and produces no hit for that story.
5. The mode cannot write: it holds a `Store` from `Store::open_read_only` and the diff adds no `INSERT`, `UPDATE` or `DELETE`.
6. `cargo run -- --export DIR` writes the same snapshot with and without a key, and `--serve`, `--poll-only` and the TUI are unchanged.
7. The eight test assertions above, plus the render assertion, pass offline with no key and no network.
8. The README names TypeSafe as the recipient of the story text, and states that `--ask` needs a key while everything else does not.

## Verification

```sh
make ci                                                       # with no key set
cargo run -- --ask "what is trending in Baku this week"        # exit 2, one message
TYPESAFE_API_KEY=... cargo run -- --ask "what is trending in Baku this week"
cargo run -- --export /tmp/site && ls /tmp/site                # unchanged, no key needed
```

The real run must report the window chosen, the stories judged, the answers, the token count and the first three rows. A `429` or a `5xx` is reported as it happened, without a retry loop.

## Open questions

1. **Thresholds.** 48 candidates, 12 stories per call, a 0.5 floor and a 20 s timeout are the design's first estimates. The first real run is the calibration, as with any physical measurement.
2. **Real-key run.** If no `TYPESAFE_API_KEY` is available at verification time, criterion 3 stays unverified and is reported as unverified. It is not simulated.
3. **`TYPESAFE_MODEL`.** Kept as an environment override. If the first run shows no use for it, drop it rather than document it.
4. **README wording.** The data section must name TypeSafe and the exact fields that leave the machine: title, lede, outlet names.
5. **Phase 2.** `jev-labels` is deferred. Its own spec is written only if the labels are wanted on the TUI, the web page and the snapshot.

## Reference corrections

`docs/jev-integration.md` was checked against the reference implementation (`F:/Programming/jev-search`) before it became this contract. The claims hold; five line citations had drifted and are corrected in place: error handling `typesafe.ts:48 → :67`, backticked state names `:95 → :107` and `:192 → :213`, percent rounding `rank.ts:34 → :41`, speculative search `pipeline.ts:107 → :119`. Verified as written: the 12 source questions and 2 candidate questions in one `inferIntent` call, `RERANK_BATCH = 40`, `SOURCE_PROB_THRESHOLD = 0.6` with the default source set as the fallback, `cachedSearch` caching engine responses only, the discarded error body, and the `0` default for a missing answer that this spec refuses to copy.
