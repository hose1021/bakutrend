# bakutrend

A terminal UI that answers one question: **what is Azerbaijan reading right now?**

It polls 20 configured Azerbaijani news sources — 10 RSS feeds and 10 Telegram channels, 19 of
them enabled — every five minutes, groups articles that describe the same event, and ranks those
events by popularity in the last hour, the last day, or the last week. The one disabled source is
`@apatv`, which began returning a preview-less stub with no post text; turn it back on in the
config when it serves posts again.

Popularity combines four inputs — two measures of reader interest, a decay for age, and how
fast the story is spreading:

- **Independent coverage** — how many outlets did the reporting. A repeat that names the
  outlet it took the story from confirms nothing and adds only to spread; a repeat that
  credits nobody keeps the older half weight, because nobody can prove which side of the
  line it is on. One outlet publishing both a feed and a channel still casts a single vote.
- **Reader engagement** — Telegram views per hour, divided by that channel's own typical
  pace so a large channel's routine post does not outrank a small channel's breakout post.
  An outlet is counted once, from its fastest post: five repeats of one story reach the same
  readers five times over.
- **Freshness** — decay from the last moment the story actually developed, which is the
  newest item from an outlet that had not carried it yet. A channel reposting the same
  headline every five minutes does not keep a dead story at the top.
- **Spread velocity** — how many outlets first picked the story up in the last 20 minutes
  (or 4 hours, in the day window; 24 hours, in the week window).

The four combine as
`0.35 × coverage + 0.35 × engagement + 0.20 × freshness + 0.10 × spread` by default; the
weights are configurable. Coverage and engagement are scaled against the 95th percentile of
the window rather than its maximum, so one outlier cannot flatten every other story's score.

Press `Enter` on any story to see every outlet carrying it, each with whether it is
independent or a repeat, its timestamp, view count, views per hour and how far above its
channel's normal pace it is — plus the raw and normalized breakdown behind the score.

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
text, `Esc` clears it · `?` toggles help · `r` forces a poll · `q` quits.

`Home` and `End` mirror `g` and `G`, and in text filter mode `Backspace` deletes one
character while `Esc` clears the whole filter.

The header reads `n/N sources ok`. N is the number of enabled sources the poller actually
polls — 19 with the shipped defaults, 20 once `@apatv` is turned back on, and whatever a
custom `sources` list enables. It is not the 20 in the default config list: a deliberately
disabled source can never report a failure, so counting it would leave the header
permanently short. The Google News seed is excluded for the same reason — it is not a
polled source.

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

Replace `CHANGEME` with your username. `bakutrend` contains no launchd code: it never
creates, loads, or unloads an agent. The plist above is entirely yours to install and
remove — nothing in the program depends on it being there.

## Configuration

The file is optional. With no `config.toml` at the default path the built-in defaults
apply; a path named with `--config` is loaded whatever it says, so a typo there is an
error rather than a silent run against the defaults.

The default path is the platform config directory, for example
`~/Library/Application Support/bakutrend/config.toml`. Any key you omit keeps its default.
A `sources` block replaces the whole default list, so copy it before editing. It also
switches off any source already in the database that it does not name, so the block is the
only thing that decides what is polled. The one source it cannot replace is the Google News
seed: that seed is not configurable, is never polled on the normal cycle, and stays enabled
because the week window is built from it. Top-level keys belong before the first `[table]`
header: TOML has no way back to the document root, so `local_keywords` written after
`[retention]` would set `retention.local_keywords` and be ignored.

```toml
poll_interval_secs = 300
cluster_threshold  = 0.40
semantic_threshold = 0.80
local_keywords     = ["Bakı", "Gəncə", "Qarabağ", "Azərbaycan"]

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

- Telegram view counts are biased by channel size. Per-channel normalization ranks
  relative buzz within a channel, not absolute reach.
- Citation detection is lexical, over feed articles, Google seeds and Telegram posts alike.
  Some reposts will be missed.
- Grouping is lexical, so two outlets describing one event with no shared vocabulary
  remain separate stories.
- Sources go dark. `haqqin.az` currently answers HTTP 418 to every request regardless of
  User-Agent, so it reports as degraded: the header counts it as one source short of the
  enabled total, and it stops contributing new articles until it recovers. It answered
  normally when the source list was built. A failing source backs off and is retried; it
  does not fail the cycle.
- A failed poll does not erase what was already fetched. Articles stored for a source stay
  in every window the source is enabled for, until they age out of the window, even while
  that source is down — only its future polls stop. This is deliberate: a transient network
  error must not silently delete data. If a dead source keeps appearing in ranked stories,
  that is why.
- The local filter is a keyword heuristic, not a geocoder.
