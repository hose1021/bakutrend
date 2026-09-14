# bakutrend

A terminal UI that answers one question: **what is Azerbaijan reading right now?**

It polls 21 configured Azerbaijani news sources — 11 RSS feeds and 10 Telegram channels, 20 of
them enabled — every five minutes, groups articles that describe the same event, and ranks those
events into a **prominence index** for the last hour, the last day, or the last week. The one
disabled source is `@apatv`, which began returning a preview-less stub with no post text; turn it
back on in the config when it serves posts again.

The index is a ranking of stories by how widely and how fast the sources this program watches
carried them. It is not a count of readers and not a probability that anyone read anything —
Telegram view counts are views, not people, and everything here is computed from what the
configured sources exposed.

The index combines four inputs — two measures of source activity, a decay for age, and how
fast the story is spreading:

- **Independent coverage** — how many outlets did the reporting. A repeat that names the
  outlet it took the story from confirms nothing and adds only to spread; a repeat that
  credits nobody keeps the older half weight, because nobody can prove which side of the
  line it is on. One outlet publishing both a feed and a channel still casts a single vote.
  No citation found means **no citation found**: the absence of a link is not evidence of
  original reporting, and the source list says `no citation seen` rather than claiming more.
- **Reader engagement** — Telegram views per hour, divided by that channel's own typical
  pace so a large channel's routine post does not outrank a small channel's breakout post.
  An outlet is counted once, from its **fastest post relative to its own channel** — an
  outlet that owns two channels of different size contributes the one that ran furthest
  above its channel's pace, not the one with the biggest raw number. Only **measured**
  paces enter the score: a rate observed between two samples at least ten minutes apart
  inside the last hour. A single-sample estimate is shown in the card but never scored, and
  the channel baseline that divides these numbers is built from measured paces alone, so
  the two sides of the ratio are the same kind of measurement.
- **Freshness** — decay from the last moment the story actually developed, which is the
  newest **first arrival** of an outlet that had not carried it yet. A channel reposting the
  same headline every five minutes does not keep a dead story at the top, and a repeat by an
  outlet that reported before the window is not counted as a fresh pickup. Both are read
  from the story's whole history rather than from the trimmed window.
- **Spread velocity** — how many outlets first picked the story up in the last 20 minutes
  (or 4 hours, in the day window; 24 hours, in the week window).

The four combine as
`0.35 × coverage + 0.35 × engagement + 0.20 × freshness + 0.10 × spread` by default; the
weights are configurable. Coverage and engagement are scaled against the 95th percentile of
the window rather than its maximum, so one outlier cannot flatten every other story's score.
Below twenty stories a nearest-rank 95th percentile **is** the maximum — `ceil(0.95n) = n`
for `n < 20` — so the code uses the maximum and says so, and the top story of a small window
is scaled to 1.0 by construction. That is a property of scaling against any order statistic,
not a claim that the story is a perfect ten.

Views per hour are reported with the state of their measurement, because they are not always
measured:

- `measured 4m ago` — an observed slope from two samples in the last hour.
- `estimate, last sample 9m ago` — one sample, or two too close together: the average since
  publication. Shown, never scored.
- `stale, last sample 41m ago` — the newest sample is older than half an hour, so the number
  describes what was last seen, not the current pace. Shown, never scored.
- No samples at all: no pace is claimed.

Nothing is inferred from a missing observation: a story whose post gained nothing in the last
hour reads as a measured zero, while a story nobody has sampled reads as no measurement.

The screen is a reading list. Ranked stories fill the left side; the story under the cursor
opens beside them — its headline, the short description its outlet published, its score with
the four inputs behind it, and every outlet carrying it.

## Install

```sh
cargo build --release
cp target/release/bakutrend ~/.local/bin/
```

The repository has a `Makefile` for the same commands. `make` on its own lists every target;
`make install` does the two steps above, and `make ci` runs the gates a commit must pass
(`fmt --all --check`, `clippy --all-targets -- -D warnings`, then the tests).

## Usage

```
bakutrend                 # the TUI
bakutrend --poll-only     # poller only, no UI
bakutrend --config PATH   # explicit config file
bakutrend --lang az       # interface language: en, az or ru
bakutrend --log [LEVEL]   # write logs to the state directory (default level: debug)
bakutrend --reset-db      # delete and rebuild the database

# Web UI
bakutrend --serve             # serve at http://127.0.0.1:8080
bakutrend --bind 0.0.0.0:9000 # serve on a chosen address (a bind implies --serve)

# A snapshot for a host that runs nothing
bakutrend --export site       # poll once, then write the site as files into site/
```

## Language

The interface speaks English, Azerbaijani (`az`) and Russian (`ru`), set with `language` in the
config file or `--lang` on the command line, which wins over the file. An unknown code is
refused at startup with the field and the three values that work, rather than quietly drawing
an English screen.

Only the interface is translated. Headlines and descriptions are shown as their outlet wrote
them — a story ranked in the Azerbaijani window is a story, not an English one. Counted nouns
follow each language's own rule, so Russian says `1 издание`, `2 издания` and `5 изданий`
rather than one form for all three.

`l` walks the interface through English, Azerbaijani and Russian without a restart, and the
status line confirms the language it landed on — in that language, so the answer is readable
even to someone who cannot read the one they just left. The switch lasts for the session: the
config file is not rewritten, and a restart returns to the configured language. While the text
filter is open `l` is filter text like every other printable key.

## The screen

The header names the program and counts the stories on screen, then the window tabs
`[ 1h ] [ 24h ] [ 7d ]` with the source health and the age of the last poll on the right. The
list ranks the stories; the card beside it explains the selected one. When the list is ordered
by something other than the index, the header says which order.

Keys: `1` `2` `3` switch window · `w` widens it · `j` `k` or arrows move · `g` `G` jump to
top/bottom · `[` `]` choose which source `Enter` opens and whose text the details view shows ·
`Enter` opens it · `s` changes the
sort · `d` opens the selected story in full, its whole published text and every source · `l`
switches the interface language · `/`
filters by text, `Esc` clears it · `?` opens help · `r` forces a poll · `q` quits. `Tab` widens
the window like `w`. Every overlay has its own keys on the footer line, and `Esc` closes what
is open before it quits.

The list has one row per story: its rank, the headline, how many outlets carry it, how it moved
since the stored ranking of one window ago, and — depending on the sort — the age of its
newest report, its view growth, or its Telegram view count. The status column holds `NEW` for a
story that was not in that stored ranking, `+2` / `-1` for one that moved, and `0` for one that
stayed. The stored ranking is one that was actually computed and written down at the time, by
the same code and with its own copy of the data; it is never recomputed later, because a
headline corrected today or a view count sampled since must not change what the past was. Until
a comparable stored ranking exists, the column is absent rather than showing a row of dashes,
and the card's movement sentence with it. A story whose headline was rewritten still matches:
identity is by word overlap, not by an exact string.

The card blocks are `PROMINENCE INDEX` with its bar, `SIGNALS` — the four inputs behind the
index, each with its normalised bar and the number the normalisation came from, and the
measurement state of the pace under `Engagement` — and `SOURCES`, every outlet carrying the
story with how it carries it, what its posts measured, and how old that measurement is. Colour
means something specific: cyan is the index, the active tab and the selected row's marker, green
is `NEW`, yellow is a notice or a failure-adjacent warning, red is a failure. No fact is carried
by colour alone: the selection has a marker column, the movement column has signs, `NEW` and
the measurement states are words, and the active tab is reversed as well as coloured. The
selected row uses the terminal's own reverse video rather than a fixed tint, so it reads the
same on a light terminal as on a dark one.

Sorts: the index (default), recent relative growth, coverage, Telegram views, and chronology.
The sort changes the order and never a score; the movement column is about the index, so it
reads the same whichever order is on screen.

On a terminal narrower than 92 columns the card drops under the list and takes the lower 40%
of the screen; the list keeps the rest, because a ranked list with no stories on it is not a
smaller screen but a broken one. On a short terminal the card's lower blocks — the signals and
the sources — are the first thing to go. `d` opens the selected story alone, filling the body
and scrolling with `j` `k` or `PageUp`/`PageDown`, so every signal and every source stays
reachable on any terminal: nothing is lost to a small window, it is one keypress away.

The details view carries the story in full. Under the headline it prints `CONTENT` and the whole
text the source published — the card shows a lede of three hundred characters, the details view
reads the store, where the text is kept as it arrived. `[` and `]` choose the source, and the
text under `CONTENT` follows the choice, because that is the source `Enter` opens: the label
names it, so the screen never sets one publication's words under another's name. Each source row
then lists that source's own headline and the exact address `Enter` will open — the address was
nowhere on screen before. A source whose feed carried no text falls back to the story's lede
under a plain `CONTENT` label, and a story with nothing stored anywhere says so
(`no text stored for this publication`) instead of showing an empty block.

A footer line lists the keys of the mode the user is in as keycaps, so the shortcut you need is
on screen without opening help. The two ways out of the mode stay pinned to the right edge, so a
narrow terminal drops a hint rather than cutting one mid-word, and rather than losing the key
that leaves. `?` opens the full list, which scrolls for the same reason the details view does:
the keys below the fold are exactly the ones a short terminal needs, and the overlay shows its
position (`2/2`) and its own scroll hint.

`Home` and `End` mirror `g` and `G`, and in text filter mode `Backspace` deletes one
character while `Esc` clears the whole filter.

A window with fewer than three stories says so — `1 story in the last hour · press w for the
last 24 hours` — and the period changes only when the user presses `w`. The program never
substitutes a wider window on its own, and never reports one or two stories as `no stories`.
A filter narrows the list without touching the period or the scores, and its notice counts what
it found: `2 of 14 matching in the last 24 hours`, or `nothing matches “külək” (14 in the last
24 hours)`.

The header reads `n/N sources ok`. N is the number of enabled sources the poller actually
polls — 19 with the shipped defaults, 20 once `@apatv` is turned back on, and whatever a
custom `sources` list enables. It is not the 20 in the default config list: a deliberately
disabled source can never report a failure, so counting it would leave the header
permanently short. The Google News seed is excluded for the same reason — it is not a
polled source.

## The web UI

`bakutrend --serve` serves the same ranking as one page and one JSON endpoint, at
`http://127.0.0.1:8080` unless `--bind` says otherwise. The poller runs beside it, so the page
keeps up with the news while it is open, and the health line at the top comes from the poller's
own report: until a cycle has finished it reads `sources unknown` and `polled never`, because
that is what this process knows.

The page is the terminal screen in HTML. The header counts what the window holds and which
window that is; the list ranks the stories with the same columns; the card explains the selected
one with `PROMINENCE INDEX`, the four `SIGNALS` and the `SOURCES`; the notice line counts a
filtered list the same way the TUI does. The score, the four inputs, the measurement states, the
movement since the previous window, the provenance of each outlet and the sentences around them
all come from `rank_window` and the same `Strings` the TUI draws, in all three languages, so the
two front ends cannot disagree about a number or a sentence. The stored text of a story is under
`CONTENT · <outlet>` in each source's own block, so no publication's words stand under another's
name.

Nothing is loaded by JavaScript. The window, the sort order, the language, the selected story and
the filter are links and one form, so every control works from a keyboard, from a screen reader
with scripting off, and the page can be served from anything that can render HTML. Which window,
order and language are in force is marked with `aria-current` as well as with colour — the same
rule the terminal follows, where no fact rests on colour alone.

The page is a masthead and two panes. The masthead holds the wordmark, the health of the poll —
a dot whose state the words beside it also state — the window and the language, and it stays at
the top, because the page is made to be left open while the poller runs. Under it: the count of
what is on screen, any notice or failure, the order with the filter, then the ranked list beside
the card. Words are set in the system interface face; every number — a rank, a count, a score, a
rate, an address — in the system monospace face, whose digits are all one width, so a column of
numbers compares by eye. Nothing is downloaded: the page uses the faces the reader's system has.
The palette is the terminal's, with the same meanings, written once as `light-dark` tokens; the
smallest text on the page is 11px, and no colour that carries text measures under 4.5:1 against
its ground in either theme.

Ordered by the index, the list is set the way it ranks: the top three stories are the largest type
on the page and the next seven a step down, because under that order the first row really is the
most prominent story. Under any other order the first row is only the first row, and every row is
set the same. Choosing a row brings its card into view, because on a narrow screen the card stands
below the whole list and a click that left the reader at the top of the ranking would have shown
them nothing. Between windows, orders and stories the browser carries the reader across with a
cross-document view transition where it supports one, and a reader who asked for less motion gets
the plain navigation.

`/api/stories?window=1h&sort=views&filter=Bakı` returns the same list as JSON, with the same
ranking and the same filter, for a script that wants the numbers rather than the page. A window
or an order the program does not know is refused with `400` instead of quietly becoming the
default one.

The server opens the database read-only for every request, so it cannot race the poller's writes
and cannot damage history: `--serve` shows the ranking, and `--poll-only` or the TUI is what
records it. Nothing is cached between requests — each one ranks the window it asked for over the
rows the store holds at that moment, so a page view costs one ranking and never shows a poll the
reader could not already see.

### A static snapshot

`bakutrend --export DIR` polls once and then writes the ranking as files that a host which runs
nothing can serve: one page per window, order and language under `DIR`, and the numbers behind each
view under `DIR/api`. Every link in a snapshot is a path to a file beside it, so the site works from
a project subdirectory such as `https://hose1021.github.io/bakutrend/` without knowing its own
address.

Two controls have no static form and are absent from a snapshot: the text filter, because there is
nothing behind a static file to run a search, and the card per story, because a card is addressed
by rank and the week window ranks over a thousand stories, so one file per rank is not a site that
can be written. A snapshot carries the card of the story its order ranks first, and every age on it
— a story's, a measurement's, the poll's — is relative to the moment printed in its masthead rather
than to the clock of whoever reads it later. Everything else is the same renderer, so a snapshot
cannot disagree with the server about a score, a notice or a sentence.

`make site` writes one into `site/`. `.github/workflows/pages.yml` publishes one to GitHub Pages
every thirty minutes: the poller runs on the runner, the database is cached between runs, and the
files are deployed as a Pages artifact. Enable it once under Settings → Pages by setting the source
to `GitHub Actions`, then run the workflow by hand to see it end to end.

Three things the runner cannot promise. GitHub's cron is best effort and starts late under load,
and scheduled workflows stop after sixty days without repository activity. Telegram may serve the
preview pages this program reads differently to a datacenter address than to a home connection.
And the database is a cache entry: if it is evicted, the next run rebuilds what it can and the week
window is empty until history accumulates again. A source that failed appears on the snapshot's own
failure line either way, so the page says what it could not reach.

## How the week window fills in

RSS feeds only reach back about a day, and Telegram exposes view counts for roughly
fourteen hours. On a database that has never completed one, the week window is seeded from
Google News' `when:7d` results, and those seeded items never appear in the 1-hour or 24-hour
windows. The seed is a one-shot job with a recorded completion: the run that finishes it
never repeats it, and a run that fails retries on the same backoff as every other source
until one succeeds. **A seed counts as finished only when it stored at least one article**: an
answer with nothing in it, or one whose every entry was refused, is reported as a failure with
the reason that fits (`the response held no articles`, `no usable articles (7 refused)`) and is
retried on that backoff, rather than leaving the week window empty behind a flag that says it
was filled. The feeds having stored their own news is not treated as the seed happening; it is
recorded separately, in the database. Real historically observed data accumulates only while
the poller runs, so for the first week the week view is coverage-weighted and partly seeded.

Every poll cycle also writes down the ranking of each window, with the moment it was computed
and the version of the arithmetic behind it. That is what the movement column compares against
one window later, and it is why the comparison cannot be polluted by anything learned since.
Rankings are kept for ten days and then pruned. A database that has never recorded a ranking
shows no movement at all, which is the honest answer: nothing comparable exists yet.

The Google seed is not a polled source: the header's source count excludes it, and it is never
fetched on the normal cycle.

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

Replace `CHANGEME` with your username. `bakutrend` contains no launchd code: it never
creates, loads, or unloads an agent. The plist above is entirely yours to install and
remove — nothing in the program depends on it being there.

## Configuration

The file is optional. With no `config.toml` at the default path the built-in defaults
apply; a path named with `--config` is loaded whatever it says, so a typo there is an
error rather than a silent run against the defaults.

The default path is the platform config directory, for example
`~/Library/Application Support/bakutrend/config.toml`. Any key you omit keeps its default.
A `sources` block replaces the whole default list, so copy it before editing.
The defaults include Oxu.az (`https://oxu.az/feed`); if you maintain a custom list, append
an RSS entry with name `Oxu.az RSS`, locator `https://oxu.az/feed`, and outlet `Oxu.az`. It also
switches off any source already in the database that it does not name, so the block is the
only thing that decides what is polled. The one source it cannot replace is the Google News
seed: that seed is not configurable, is never polled on the normal cycle, and stays enabled
because the week window is built from it. Top-level keys belong before the first `[table]`
header: TOML has no way back to the document root, so `language` written after
`[retention]` would set `retention.language` and be ignored.

```toml
poll_interval_secs = 300
cluster_threshold  = 0.40
semantic_threshold = 0.80
language           = "en"

[weights]
coverage        = 0.35
engagement      = 0.35
freshness       = 0.20
spread_velocity = 0.10

[retention]
view_sample_days = 30

[[sources]]
name    = "Qafqazinfo RSS"
kind    = "rss"          # rss | telegram | google
locator = "https://qafqazinfo.az/rss"
outlet  = "Qafqazinfo"   # one outlet may own several sources
enabled = true
```

`cluster_threshold` is Jaccard similarity over headline words: two items merge at or above
it, but only when they also share a named entity, so two sentences about different events in
the same city stay apart. `semantic_threshold` is the cosine similarity at which an
embedding provider may merge two headlines that share no word at all; it has no effect until
vectors exist in the database (see *Semantic similarity* below).

Every number in the file is checked before the database is opened. A value that parses but
cannot mean anything is refused with the field and the range it should have been in: a
threshold outside 0–1 or not a number at all, a negative or non-finite weight, a weight set
that sums to zero, a retention of zero days, or a retention long enough to overflow when days
become seconds. Weights do not have to sum to one — the score divides by their sum — so
re-weighting one signal above the rest is allowed.

Articles are kept forever. The only thing retention prunes is Telegram view samples,
after 30 days.

## Semantic similarity (not wired up)

`src/embed.rs` defines the `EmbeddingProvider` interface, the cosine and centroid helpers and
a SQLite-backed vector cache, and `embed::embed_pending` is the job that fills that cache.
The clustering rule that consumes it is live and tested: an item with a vector is compared
against each group's centroid, and joins at `semantic_threshold`.

**It does nothing in a default build, because no provider is implemented.** There is no HTTP
client for an embedding API in this program and no API key handling; with no vectors in the
`embeddings` table the clusterer decides purely lexically, which is exactly what the
`cluster_threshold` rule describes. Adding a provider means implementing one trait method and
calling `embed_pending` once per poll with a writer connection — see the module docs in
`src/embed.rs`. Nothing else changes, and the app picks the vectors up automatically.

### Paths on macOS

| What | Where |
|---|---|
| Config | `~/Library/Application Support/bakutrend/config.toml` |
| Database | `~/Library/Application Support/bakutrend/bakutrend.sqlite` |
| Log (`--log`) | `~/Library/Application Support/bakutrend/bakutrend.log` |

The log lives in the state directory, which on macOS falls back to the data directory,
so it sits beside the database. The platform cache directory,
`~/Library/Caches/bakutrend/`, is resolved but never written to — there is nothing to
clear there.

## Known limits

- Telegram view counts are biased by channel size, and they are **views, not people**: nothing
  here counts unique readers. Per-channel normalization ranks relative buzz within a channel,
  not absolute reach.
- Citation detection is lexical, over a headline and body of feed articles, Google seeds and
  Telegram posts alike. Some reposts will be missed, and a citation that uses a phrase the
  marker table does not know is one of them. A missing citation is only a missing citation: it
  is never read as proof of original reporting.
- The marker table is small and hand-written for Azerbaijani and Russian. Adding a phrase is a
  one-line change in `src/text.rs`, and `source::is_cited` and `text::cited_outlet` both read
  the same table.
- Grouping is lexical. Two outlets describing one event with no shared vocabulary, and typically
  two outlets writing in different languages, remain separate stories. The gazetteer aligns the
  spellings of about sixty countries, cities and organizations, which helps the *evidence* gate
  but cannot align words; a semantic step is stubbed out (`Semantic similarity` above) and does
  nothing in a default build. A shared city is deliberately not enough to merge two stories —
  the cause is the news, and two different causes in one city are two events.
- Measurements are bounded by what the sources expose. Telegram samples arrive at most once per
  ten minutes per item, so a pace is measured only after about twenty minutes of following a
  post; before that the card shows an estimate. A story whose subject is older than the two
  windows of grouping context (`2 × window`) is read from the edge of that context: an outlet
  whose first report is older than the context looks like a first arrival at the edge.
- Snapshots of rankings are written by the poll cycle, so periods nobody ran the program through
  have no stored ranking and no movement to report. Ten days of rankings are kept.
- The scaling percentile is a small-sample compromise with one measured side effect. Below
  twenty stories in a window the 95th percentile *is* the maximum, so the top story is scaled to
  1.0 by construction and every other story is measured against it. At twenty stories and above
  everything at or above the percentile also normalises to 1.0, so the top one or two stories
  share the maximum and are ordered by the recency tie-break. That tie is bounded, and the
  alternative — dividing by the maximum — would let one outlier push every other story toward
  zero. Both effects are asserted in `the_normalizer_decides_only_who_shares_the_top`. On a
  quiet hour the window usually holds one story: treat the index of a thin window as an
  ordering, not a percentage, and read the story count on the header.
- The details view prints the text the feed published, not the article page: this program never
  fetches an article body, and several feeds publish only a truncated summary (Qafqazinfo's end
  in an ellipsis of its own; its items also carry an `Ətraflı:` link as plain body text). A body
  wrapped in a CDATA section is stripped before it is stored (`text::plain_text`, tested against
  the escaped form that feed sends), and markup inside a body is dropped, with a space where a
  tag stood: `başlayır<br>Ətraflı:` is two words, and Haqqin.az's `<p><img …></p>Президент`
  arrives as the sentence it is. A `<` that starts no tag is the publisher's own character and
  stays — `Artım <1%` is not an element.
- Sources go dark. A source whose locator stops answering reports as degraded: the header counts
  it as one source short of the enabled total, and it stops contributing new articles until it
  recovers. It backs off and is retried; it does not fail the cycle. The list has one such trap in
  it already: `haqqin.az` answers `/rss` and `/rss.xml` with the feed and HTTP 418 for `/rss/`,
  which is why the shipped locator carries no trailing slash. A locator is not a page a reader
  types, and the trailing slash is the difference between a feed and nothing.
- A failed poll does not erase what was already fetched. Articles stored for a source stay
  in every window the source is enabled for, until they age out of the window, even while
  that source is down — only its future polls stop. This is deliberate: a transient network
  error must not silently delete data. If a dead source keeps appearing in ranked stories,
  that is why.
- A selected row is drawn with reverse video and a marker column. On a terminal where the
  reverse attribute is ignored the marker still marks the row, which is why it exists.
