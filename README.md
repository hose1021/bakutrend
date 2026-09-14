# bakutrend

A terminal UI that answers one question: **what is Azerbaijan reading right now?**

It polls 20 configured Azerbaijani news sources — 10 RSS feeds and 10 Telegram channels, 19 of
them enabled — every five minutes, groups articles that describe the same event, and ranks those
events by popularity in the last hour, the last day, or the last week. The one disabled source is
`@apatv`, which began returning a preview-less stub with no post text; turn it back on in the
config when it serves posts again.

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
text, `Esc` clears it · `?` toggles help · `r` forces a poll · `q` quits.

The header reads `n/19 sources ok`. The denominator is the number of enabled sources the
poller actually polls, not the 20 in the config list: a deliberately disabled source can
never report a failure, so counting it would leave the header permanently short. The
Google News seed is excluded for the same reason — it is not a polled source.

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

Articles are kept forever. The only thing retention prunes is Telegram view samples,
after 30 days.

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
- Citation detection is lexical. Some reposts will be missed.
- Grouping is lexical, so two outlets describing one event with no shared vocabulary
  remain separate stories.
- Sources go dark. `haqqin.az` currently answers HTTP 418 to every request regardless of
  User-Agent, so it reports as degraded: the header counts it as one source short of 19,
  and it contributes no articles until it recovers. It answered normally when the source
  list was built. A failing source backs off and is retried; it does not fail the cycle.
- The local filter is a keyword heuristic, not a geocoder.
