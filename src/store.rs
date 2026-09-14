//! SQLite persistence. `items` and `view_samples` are the only sources of truth;
//! stories are derived in memory by `cluster` and `score`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use crate::error::StoreError;
use crate::source::{ParsedItem, SourceKind, SourceSpec};
use crate::text::fold;

/// Identity of an outlet, ignoring case, punctuation and site suffixes, so
/// `Report.az`, `www.report.az` and `report` are one outlet.
pub fn outlet_key(name: &str) -> String {
    let brand = name.strip_prefix("www.").unwrap_or(name);
    fold(brand.split('.').next().unwrap_or(brand))
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
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
    /// The outlet this item credits, folded, when a citation marker named one. `None` means
    /// either that the item is original reporting or that the citation does not say who it
    /// credits — `cited` distinguishes those two, and coverage treats them differently.
    pub cited_outlet: Option<String>,
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

    /// The recent sub-window over which a story's spread is measured. Deliberately not one
    /// fixed ratio of the window: inside an hour, twenty minutes is the news cycle, while
    /// inside a day four hours is already old news, and a day is what makes a week's spread
    /// current.
    pub fn spread_seconds(self) -> i64 {
        match self {
            Window::Hour => 20 * 60,
            Window::Day => 4 * 3600,
            Window::Week => 24 * 3600,
        }
    }

    pub fn all() -> [Window; 3] {
        [Window::Hour, Window::Day, Window::Week]
    }

    /// The next wider period, with the week as its own end: a wider window than the week is not
    /// accumulated here, and pretending otherwise would show less history under a longer name.
    pub fn wider(self) -> Self {
        match self {
            Window::Hour => Window::Day,
            Window::Day | Window::Week => Window::Week,
        }
    }
}

/// One story's place in a stored ranking.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotStory {
    pub key: String,
    pub rank: i64,
    pub score: f64,
}

/// A ranking as it was computed at one moment.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// When the ranking was calculated, not when it was read back.
    pub computed_at: i64,
    /// Stories in rank order, best first.
    pub stories: Vec<SnapshotStory>,
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
  cited_outlet TEXT,
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

-- Optional semantic similarity. Keyed by model as well as item, because two models produce
-- vectors in different spaces and a single cached vector would silently compare across them.
CREATE TABLE IF NOT EXISTS embeddings (
  item_id    INTEGER NOT NULL REFERENCES items(id),
  model      TEXT NOT NULL,
  dim        INTEGER NOT NULL,
  vector     BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (item_id, model)
);

-- Rankings as they were computed, so a later run can compare against something that really was
-- shown. Recomputing a past ranking from today's rows leaks today's headlines, citations and
-- view counts into a claim about yesterday: an article first seen this morning would change
-- what the ranking "was" last night. `algorithm_version` keeps two different scoring rules
-- apart, and `computed_at` is when the numbers were actually calculated.
CREATE TABLE IF NOT EXISTS ranking_snapshots (
  computed_at       INTEGER NOT NULL,
  algorithm_version INTEGER NOT NULL,
  window_hours      INTEGER NOT NULL,
  story_key         TEXT NOT NULL,
  rank              INTEGER NOT NULL,
  score             REAL NOT NULL,
  PRIMARY KEY (computed_at, window_hours, story_key)
);
CREATE INDEX IF NOT EXISTS ranking_snapshots_lookup
  ON ranking_snapshots(window_hours, algorithm_version, computed_at);

-- State that has to outlive the process, as opposed to state that can be recomputed from
-- `items` and `view_samples`. A fresh table here reaches a database created by an earlier
-- version too: `CREATE TABLE IF NOT EXISTS` is re-run on every open, unlike a column, which
-- needs its own guarded `ALTER TABLE` in `migrate`.
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

/// Additive migrations for a database created by an earlier version. `CREATE TABLE IF NOT
/// EXISTS` never alters an existing table, so a column added to [`SCHEMA`] reaches a fresh
/// database and no other; every addition needs its own guarded `ALTER TABLE` here. The
/// column is nullable and every reader treats `NULL` as "unknown", so an unmigrated row
/// keeps working instead of failing.
fn migrate(conn: &Connection) -> Result<(), StoreError> {
    let mut stmt = conn.prepare("PRAGMA table_info(items)")?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    if !columns.iter().any(|name| name == "cited_outlet") {
        conn.execute_batch("ALTER TABLE items ADD COLUMN cited_outlet TEXT")?;
    }
    Ok(())
}

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

    /// Spec §6.2: the UI reads on a read-only connection. No pragmas, no schema batch:
    /// both are writes, and the poller's `Store::open` has already prepared the database.
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self { conn })
    }
    fn prepare(conn: Connection) -> Result<Self, StoreError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Resolve an outlet by exact folded key, creating it when absent. `outlet_key`
    /// already reduces `Report.az` to `report`, so an exact match is enough; a prefix
    /// fallback would merge distinct outlets (`Baku Post` into `Baku.ws`).
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
            let Some(kind) = SourceKind::parse(&kind) else {
                continue;
            };
            let enabled = enabled != 0;
            if only_enabled && !enabled {
                continue;
            }
            out.push(SourceRow {
                id,
                kind,
                name,
                locator,
                outlet,
                enabled,
            });
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
                                    section, published_at, first_seen, last_seen, views, cited,
                                    cited_outlet, is_backfill)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11, ?12, ?13)
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
                   cited_outlet = excluded.cited_outlet,
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
                    item.cited_outlet,
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
                .query_row("SELECT kind FROM sources WHERE id = ?1", [source_id], |r| {
                    r.get(0)
                })?;
        Ok(SourceKind::parse(&raw).unwrap_or(SourceKind::Rss))
    }

    pub fn item_count(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get(0))?)
    }

    /// Items and the view samples belonging to those items, for a time range.
    /// Samples are selected by their item, not their own timestamp, so a sample taken
    /// outside the range still describes an item inside it.
    ///
    /// An item whose `published_at` falls inside the range but which the program only discovered
    /// later is left out: the range is a claim about what was knowable at its end, and a story
    /// the program had never seen cannot have been part of it.
    pub fn window_data(
        &self,
        from: i64,
        to: i64,
        allow_backfill: bool,
    ) -> Result<(Vec<ItemRow>, Vec<Sample>), StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT i.id, i.source_id, i.outlet_id, o.name, s.kind, i.title, i.description,
                    i.url, i.published_at, i.views, i.cited, i.cited_outlet, i.is_backfill
             FROM items i
             JOIN sources s ON s.id = i.source_id
             JOIN outlets o ON o.id = i.outlet_id
             WHERE i.published_at >= ?1 AND i.published_at <= ?2
               AND i.first_seen <= ?2
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
                r.get::<_, Option<String>>(11)?,
                r.get::<_, i64>(12)?,
            ))
        })?;
        let mut items = Vec::new();
        for row in mapped {
            let (
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
                cited,
                cited_outlet,
                is_backfill,
            ) = row?;
            let Some(kind) = SourceKind::parse(&kind) else {
                continue;
            };
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
                cited_outlet,
                is_backfill: is_backfill != 0,
            });
        }

        let mut sample_stmt = self.conn.prepare(
            "SELECT v.item_id, v.ts, v.views
             FROM view_samples v
             JOIN items i ON i.id = v.item_id
             JOIN sources s ON s.id = i.source_id
            WHERE i.published_at >= ?1 AND i.published_at <= ?2
              AND i.first_seen <= ?2
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

    /// A value written by an earlier run, keyed by a name the writer chose.
    ///
    /// `meta` exists for the few facts that cannot be derived from `items` and `view_samples` —
    /// a one-shot job that already ran, for instance. Absent is the normal answer for a database
    /// that predates the key, and callers must treat it as "not done yet" rather than an error.
    pub fn meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    /// Record a fact for later runs. `ON CONFLICT` makes a repeat idempotent, so a crashed run
    /// that repeats its last step cannot end up with two values under one key.
    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// The model of the most recently written vector, or `None` when the cache is empty.
    ///
    /// The cache names itself so that nothing needs configuring for it to be used, and so two
    /// models are never loaded into one comparison by accident — vectors from different models
    /// live in different spaces and their cosine is meaningless.
    pub fn current_embedding_model(&self) -> Result<Option<String>, StoreError> {
        let model = self
            .conn
            .query_row(
                "SELECT model FROM embeddings ORDER BY created_at DESC, rowid DESC LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(model)
    }

    /// Cached embedding vectors for the items of a range, for one model.
    ///
    /// An empty map is the normal answer, not an error: vectors are written by the poller
    /// after ingestion, and a database that has never run with a provider has none. Every
    /// caller must therefore treat a missing vector as "cluster this item lexically".
    pub fn embeddings_for_range(
        &self,
        from: i64,
        to: i64,
        model: &str,
    ) -> Result<HashMap<i64, Vec<f32>>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT e.item_id, e.vector
             FROM embeddings e
             JOIN items i ON i.id = e.item_id
             WHERE i.published_at >= ?1 AND i.published_at <= ?2 AND e.model = ?3",
        )?;
        let rows = stmt.query_map(rusqlite::params![from, to, model], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (item_id, bytes) = row?;
            // A vector that no longer decodes is one corrupt row, not a reason to fail the
            // whole refresh: that item simply clusters lexically.
            if let Some(vector) = crate::embed::from_bytes(&bytes) {
                out.insert(item_id, vector);
            }
        }
        Ok(out)
    }

    /// Store vectors for one model. `INSERT OR REPLACE` makes a re-embed idempotent rather
    /// than an error, so a crashed batch can simply be repeated.
    pub fn save_embeddings(
        &mut self,
        model: &str,
        rows: &[(i64, Vec<f32>)],
        now: i64,
    ) -> Result<usize, StoreError> {
        let tx = self.conn.transaction()?;
        for (item_id, vector) in rows {
            tx.execute(
                "INSERT OR REPLACE INTO embeddings (item_id, model, dim, vector, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    item_id,
                    model,
                    vector.len() as i64,
                    crate::embed::to_bytes(vector),
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(rows.len())
    }

    /// Texts still lacking a vector for `model`, newest first, capped at `limit`.
    ///
    /// Newest first because the short windows rank on recent items: embedding the newest
    /// rows makes the semantic path useful immediately, while an oldest-first backfill would
    /// spend its budget on history the user is not looking at. The cap is what keeps one
    /// cycle's work bounded when a database is far behind.
    pub fn items_missing_embeddings(
        &self,
        model: &str,
        limit: usize,
    ) -> Result<Vec<(i64, String)>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT i.id, i.title, i.description
             FROM items i
             WHERE NOT EXISTS (
                     SELECT 1 FROM embeddings e WHERE e.item_id = i.id AND e.model = ?1
                   )
             ORDER BY i.published_at DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![model, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (item_id, title, description) = row?;
            out.push((
                item_id,
                match description {
                    Some(text) if !text.is_empty() => format!("{title}. {text}"),
                    _ => title,
                },
            ));
        }
        Ok(out)
    }

    /// Record the current view count of every Telegram item, at most once per ten minutes
    /// per item. Telegram only exposes counts for a limited window, so a missed sample is lost.
    pub fn sample_views(&mut self, now: i64) -> Result<usize, StoreError> {
        let inserted = self.conn.execute(
            "INSERT INTO view_samples (item_id, ts, views)
             SELECT i.id, ?1, i.views
             FROM items i JOIN sources s ON s.id = i.source_id
             WHERE s.kind = 'telegram' AND i.views IS NOT NULL AND i.last_seen = ?1
               AND NOT EXISTS (
                 SELECT 1 FROM view_samples v WHERE v.item_id = i.id AND v.ts > ?1 - 600
               )",
            [now],
        )?;
        Ok(inserted)
    }

    pub fn prune_samples(&mut self, before: i64) -> Result<usize, StoreError> {
        Ok(self
            .conn
            .execute("DELETE FROM view_samples WHERE ts < ?1", [before])?)
    }

    /// Record a ranking as it was just computed, with the version of the arithmetic behind it.
    ///
    /// `stories` is `(key, score)` in rank order; the index in that slice is the rank. A repeat
    /// write for one `(computed_at, window)` replaces the earlier rows rather than failing, so a
    /// caller that recomputes the same moment cannot end up with two ranks for one story.
    pub fn save_snapshot(
        &mut self,
        computed_at: i64,
        algorithm_version: i64,
        window: Window,
        stories: &[(String, f64)],
    ) -> Result<usize, StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM ranking_snapshots WHERE computed_at = ?1 AND window_hours = ?2",
            rusqlite::params![computed_at, window.hours()],
        )?;
        for (rank, (key, score)) in stories.iter().enumerate() {
            tx.execute(
                "INSERT INTO ranking_snapshots
                   (computed_at, algorithm_version, window_hours, story_key, rank, score)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    computed_at,
                    algorithm_version,
                    window.hours(),
                    key,
                    rank as i64,
                    score
                ],
            )?;
        }
        tx.commit()?;
        Ok(stories.len())
    }

    /// The stored ranking of `window` closest to `at`, computed by `algorithm_version` and no
    /// further away than `tolerance`.
    ///
    /// `None` means there is nothing comparable: a window that has never been ranked, a ranking
    /// computed by different arithmetic, or one too far from the moment asked about. Every caller
    /// must treat that as "no comparison", never as "nothing changed".
    pub fn snapshot_near(
        &self,
        window: Window,
        algorithm_version: i64,
        at: i64,
        tolerance: i64,
    ) -> Result<Option<Snapshot>, StoreError> {
        let computed_at: Option<i64> = self
            .conn
            .query_row(
                "SELECT computed_at FROM ranking_snapshots
                  WHERE window_hours = ?1 AND algorithm_version = ?2
                    AND computed_at BETWEEN ?3 - ?4 AND ?3 + ?4
                  ORDER BY ABS(computed_at - ?3) ASC, computed_at DESC
                  LIMIT 1",
                rusqlite::params![window.hours(), algorithm_version, at, tolerance.max(0)],
                |r| r.get(0),
            )
            .optional()?;
        let Some(computed_at) = computed_at else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT story_key, rank, score FROM ranking_snapshots
              WHERE computed_at = ?1 AND window_hours = ?2
              ORDER BY rank ASC",
        )?;
        let stories = stmt
            .query_map(rusqlite::params![computed_at, window.hours()], |r| {
                Ok(SnapshotStory {
                    key: r.get(0)?,
                    rank: r.get(1)?,
                    score: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(Snapshot {
            computed_at,
            stories,
        }))
    }

    /// The newest stored ranking time for a window, or `None` when it has never been ranked.
    pub fn latest_snapshot_at(&self, window: Window) -> Result<Option<i64>, StoreError> {
        Ok(self.conn.query_row(
            "SELECT MAX(computed_at) FROM ranking_snapshots WHERE window_hours = ?1",
            [window.hours()],
            |r| r.get::<_, Option<i64>>(0),
        )?)
    }

    /// The fullest text stored for each of `urls`, keyed by URL.
    ///
    /// A ranked story carries a lede — the newest body cut at three hundred characters — because
    /// a list row has room for nothing longer. The store kept what the sources published, and
    /// the details view is where the whole of it belongs.
    ///
    /// One article can arrive more than once, through Google News and through the outlet's own
    /// feed, and both rows hold the same URL. The longest text wins: one column holds a summary
    /// and a full body alike, and of the two the longer one is the article.
    pub fn bodies_by_url(&self, urls: &[String]) -> Result<HashMap<String, String>, StoreError> {
        let mut bodies: HashMap<String, String> = HashMap::new();
        if urls.is_empty() {
            return Ok(bodies);
        }
        // The placeholder list is built here because SQLite takes one parameter per URL and the
        // count is only known at run time.
        let placeholders = std::iter::repeat_n("?", urls.len())
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT url, description FROM items
              WHERE url IN ({placeholders}) AND description IS NOT NULL"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(urls.iter()), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (url, body) = row?;
            // A feed that ships an empty summary is not a source with text. Returning it would
            // hand the details view a blank block and hide the note that says why.
            if body.trim().is_empty() {
                continue;
            }
            let longer = bodies.get(&url).is_none_or(|kept| body.len() > kept.len());
            if longer {
                bodies.insert(url, body);
            }
        }
        Ok(bodies)
    }

    /// Drop stored rankings older than `before`. They are history the movement column no longer
    /// reaches; keeping them forever would grow the database without ever being read.
    pub fn prune_snapshots(&mut self, before: i64) -> Result<usize, StoreError> {
        Ok(self.conn.execute(
            "DELETE FROM ranking_snapshots WHERE computed_at < ?1",
            [before],
        )?)
    }
}

/// Single outlet lookup, shared by `resolve_outlet` and the upsert transaction
/// (`Transaction` derefs to `Connection`, so both callers use this one query).
fn resolve_outlet_in(conn: &Connection, name: &str) -> Result<i64, StoreError> {
    let key = outlet_key(name);
    let existing: Option<i64> = conn
        .query_row("SELECT id FROM outlets WHERE key = ?1", [&key], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO outlets (name, key) VALUES (?1, ?2)",
        rusqlite::params![name, key],
    )?;
    Ok(conn.last_insert_rowid())
}

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
            cited_outlet: None,
            publisher: None,
        }
    }

    fn fixture() -> (Store, i64) {
        let mut store = Store::open_in_memory().expect("memory store");
        let source_id = store
            .ensure_source(&spec("APA", "APA RSS", "https://apa.az/rss"), true)
            .unwrap();
        (store, source_id)
    }

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
    fn upserting_the_same_payload_twice_inserts_nothing_the_second_time() {
        let (mut store, source_id) = fixture();
        let items = vec![item("a", "Bakıda yollar bağlıdır", 1_700_000_000)];

        assert_eq!(
            store
                .upsert_items(source_id, &items, 1_700_000_000)
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .upsert_items(source_id, &items, 1_700_000_300)
                .unwrap(),
            0
        );
        assert_eq!(store.item_count().unwrap(), 1);
    }

    #[test]
    fn upsert_refreshes_last_seen_without_touching_published_at() {
        let (mut store, source_id) = fixture();
        let items = vec![item("a", "Başlıq", 1_700_000_000)];
        store
            .upsert_items(source_id, &items, 1_700_000_000)
            .unwrap();
        store
            .upsert_items(source_id, &items, 1_700_000_900)
            .unwrap();

        let first_seen: i64 = store
            .conn
            .query_row(
                "SELECT first_seen FROM items WHERE external_id = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let last_seen: i64 = store
            .conn
            .query_row(
                "SELECT last_seen FROM items WHERE external_id = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(first_seen, 1_700_000_000);
        assert_eq!(last_seen, 1_700_000_900);
    }

    /// The details view reads whole texts, and a ranked story holds only a lede. The same URL can
    /// arrive through two sources, and one column holds both a summary and a full body: the
    /// longer text is the article, and it is the one the reader is owed.
    #[test]
    fn stored_bodies_come_back_whole_and_the_longest_wins() {
        let (mut store, source_id) = fixture();
        let mut short = item("a", "Başlıq", 1_700_000_000);
        short.description = Some("Qısa xülasə.".into());
        let mut long = item("b", "Başlıq", 1_700_000_000);
        long.url = short.url.clone();
        long.description = Some("Tam mətn. ".repeat(80));
        let mut none = item("c", "Başlıqsız mətn", 1_700_000_000);
        none.description = None;
        // A feed that ships an empty summary is not a source with text either.
        let mut blank = item("d", "Boş mətn", 1_700_000_000);
        blank.description = Some("   \n".into());
        store
            .upsert_items(source_id, &[short, long, none, blank], 1_700_000_000)
            .unwrap();

        let urls = vec![
            "https://example.az/a".to_string(),
            "https://example.az/c".to_string(),
            "https://example.az/d".to_string(),
            "https://example.az/missing".to_string(),
        ];
        let bodies = store.bodies_by_url(&urls).unwrap();
        assert_eq!(bodies.len(), 1, "only the URLs with text are returned");
        let body = &bodies["https://example.az/a"];
        assert!(body.starts_with("Tam mətn."), "{body}");
        assert_eq!(body.chars().count(), 800, "the whole stored text, uncut");
        assert!(!bodies.contains_key("https://example.az/c"));
        assert!(!bodies.contains_key("https://example.az/d"));
        assert!(store.bodies_by_url(&[]).unwrap().is_empty());
    }

    #[test]
    fn one_outlet_publishing_two_feeds_is_one_outlet() {
        let mut store = Store::open_in_memory().unwrap();
        let rss = store
            .ensure_source(
                &spec("Qafqazinfo", "Qafqazinfo RSS", "https://qafqazinfo.az/rss"),
                true,
            )
            .unwrap();
        let tg = store
            .ensure_source(
                &spec("Qafqazinfo", "Qafqazinfo Telegram", "@qafqazinfo"),
                true,
            )
            .unwrap();
        assert_ne!(rss, tg, "sources are distinct rows");
        let outlets: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM outlets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(outlets, 1, "both sources belong to one outlet");
    }

    #[test]
    fn google_items_credit_the_named_publisher_not_the_seed_source() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .ensure_source(
                &spec("Report", "Report RSS", "https://report.az/rss/"),
                true,
            )
            .unwrap();
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
        store
            .upsert_items(seed, &[backfill], 1_700_000_000)
            .unwrap();

        let credited: String = store
            .conn
            .query_row(
                "SELECT o.name FROM items i JOIN outlets o ON o.id = i.outlet_id WHERE i.external_id = 'g1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            credited, "Report",
            "Report.az must resolve to the existing Report outlet"
        );
    }

    #[test]
    fn distinct_outlets_whose_keys_are_prefix_related_never_merge() {
        fn outlet_count(order: [&str; 2]) -> i64 {
            let mut store = Store::open_in_memory().unwrap();
            for (i, outlet) in order.iter().enumerate() {
                let locator = format!("https://example.az/{i}/rss");
                store
                    .ensure_source(&spec(outlet, outlet, &locator), true)
                    .unwrap();
            }
            store
                .conn
                .query_row("SELECT COUNT(*) FROM outlets", [], |r| r.get(0))
                .unwrap()
        }

        assert_eq!(outlet_count(["Baku.ws", "Baku Post"]), 2);
        assert_eq!(outlet_count(["Baku Post", "Baku.ws"]), 2);
        assert_eq!(outlet_count(["APA", "Apa TV"]), 2);
        assert_eq!(outlet_count(["Apa TV", "APA"]), 2);
    }

    #[test]
    fn a_later_publisher_moves_the_item_to_the_resolved_outlet() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .ensure_source(
                &spec("Report", "Report RSS", "https://report.az/rss/"),
                true,
            )
            .unwrap();
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

        store
            .upsert_items(
                seed,
                &[item("g1", "Bakıda yol qəzası", 1_700_000_000)],
                1_700_000_000,
            )
            .unwrap();
        let mut attributed = item("g1", "Bakıda yol qəzası", 1_700_000_000);
        attributed.publisher = Some("Report.az".to_string());
        store
            .upsert_items(seed, &[attributed], 1_700_000_600)
            .unwrap();

        let credited: String = store
            .conn
            .query_row(
                "SELECT o.name FROM items i JOIN outlets o ON o.id = i.outlet_id WHERE i.external_id = 'g1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            credited, "Report",
            "a publisher that arrives late must reattribute the item"
        );
    }

    #[test]
    fn stored_views_never_go_backwards() {
        fn stored(store: &Store) -> Option<i64> {
            store
                .conn
                .query_row("SELECT views FROM items WHERE external_id = 'a'", [], |r| {
                    r.get(0)
                })
                .unwrap()
        }

        let (mut store, source_id) = fixture();
        let mut payload = item("a", "Başlıq", 1_700_000_000);

        payload.views = Some(100);
        store
            .upsert_items(source_id, &[payload.clone()], 1_700_000_000)
            .unwrap();
        assert_eq!(stored(&store), Some(100));

        payload.views = Some(90);
        store
            .upsert_items(source_id, &[payload.clone()], 1_700_000_060)
            .unwrap();
        assert_eq!(
            stored(&store),
            Some(100),
            "a stale lower count must not overwrite a higher one"
        );

        payload.views = Some(150);
        store
            .upsert_items(source_id, &[payload.clone()], 1_700_000_120)
            .unwrap();
        assert_eq!(stored(&store), Some(150));

        payload.views = None;
        store
            .upsert_items(source_id, &[payload.clone()], 1_700_000_180)
            .unwrap();
        assert_eq!(
            stored(&store),
            Some(150),
            "a parse without views must not erase them"
        );

        payload.views = None;
        store
            .upsert_items(
                source_id,
                &[item("b", "Views yoxdur", 1_700_000_000)],
                1_700_000_240,
            )
            .unwrap();
        let no_views: Option<i64> = store
            .conn
            .query_row("SELECT views FROM items WHERE external_id = 'b'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(no_views, None, "items that never carried views stay NULL");
    }

    #[test]
    fn outlet_key_folds_and_strips_punctuation() {
        assert_eq!(outlet_key("Report.az"), outlet_key("report"));
        assert_eq!(outlet_key("APA"), outlet_key("apa"));
        // The same outlet written with its site prefix and with none: coverage counts outlets,
        // so two spellings of one outlet would count as two independent reporters.
        assert_eq!(outlet_key("www.report.az"), outlet_key("Report"));
        assert_eq!(outlet_key("Baku.ws"), outlet_key("baku"));
        // Different outlets stay different: a prefix fallback would merge these two.
        assert_ne!(outlet_key("Baku Post"), outlet_key("Baku.ws"));
    }

    #[test]
    fn window_excludes_backfill_for_short_windows_only() {
        let mut store = Store::open_in_memory().unwrap();
        let rss = store
            .ensure_source(&spec("APA", "APA RSS", "https://apa.az/rss"), true)
            .unwrap();
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
        store
            .upsert_items(rss, &[item("live", "Canlı xəbər", now - 60)], now)
            .unwrap();
        store
            .upsert_items(seed, &[item("seed", "Köhnə xəbər", now - 3600)], now)
            .unwrap();

        let (day_items, _) = store.window(Window::Day, now).unwrap();
        assert_eq!(
            day_items.len(),
            1,
            "backfill must not appear in the 24h window"
        );
        assert_eq!(day_items[0].title, "Canlı xəbər");

        let (week_items, _) = store.window(Window::Week, now).unwrap();
        assert_eq!(week_items.len(), 2, "backfill is admitted in the 7d window");
    }

    #[test]
    fn windowed_items_carry_their_outlet_and_kind() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store
            .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now)
            .unwrap();

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
        store
            .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now)
            .unwrap();
        store.set_enabled(id, false).unwrap();
        let (rows, _) = store.window(Window::Hour, now).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn samples_are_recorded_once_per_throttle_window() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store
            .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now)
            .unwrap();

        assert_eq!(store.sample_views(now).unwrap(), 1);
        // Each later sample needs its own poll first (`last_seen = now`); once the item
        // has been touched, the ten-minute throttle still applies to it.
        store
            .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now + 300)
            .unwrap();
        assert_eq!(store.sample_views(now + 300).unwrap(), 0);
        store
            .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now + 900)
            .unwrap();
        assert_eq!(store.sample_views(now + 900).unwrap(), 1);
    }

    /// Only rows this poll actually touched (`last_seen = now`) may be sampled: a source
    /// that failed keeps its stale views and must not manufacture a flat velocity.
    #[test]
    fn sample_views_only_samples_rows_the_poll_touched() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store
            .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now - 3600)
            .unwrap();

        // No poll at `now`: nothing is sampled.
        assert_eq!(store.sample_views(now).unwrap(), 0);

        store
            .upsert_items(id, &[telegram_item("p1", 700, now - 300)], now)
            .unwrap();
        assert_eq!(store.sample_views(now).unwrap(), 1);
    }

    #[test]
    fn samples_are_read_back_for_the_window_that_owns_the_item() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store
            .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now)
            .unwrap();
        store.sample_views(now).unwrap();

        let (_, samples) = store.window(Window::Hour, now).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].views, 500);
    }

    /// Spec §6.2: the UI reads on a read-only connection, so it cannot contend with the
    /// poller for the write lock.
    #[test]
    fn a_read_only_store_can_query_but_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro.sqlite");
        let now = 1_700_000_000;
        let id = {
            let mut store = Store::open(&path).unwrap();
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
            store
                .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now)
                .unwrap();
            id
        };

        let mut reader = Store::open_read_only(&path).unwrap();
        let (rows, _) = reader.window(Window::Hour, now).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "the read-only connection serves window queries"
        );
        assert!(
            reader
                .upsert_items(id, &[telegram_item("p2", 600, now - 300)], now)
                .is_err(),
            "a read-only connection must reject writes"
        );
    }

    #[test]
    fn prune_removes_only_samples_older_than_the_cutoff() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store
            .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now)
            .unwrap();
        store.sample_views(now).unwrap();
        store
            .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now + 900)
            .unwrap();
        store.sample_views(now + 900).unwrap();

        assert_eq!(store.prune_samples(now + 600).unwrap(), 1);
        let remaining: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM view_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn a_credited_repost_keeps_the_outlet_it_names() {
        let (mut store, id) = fixture();
        let mut credited = item("c1", "Yanğın söndürülüb", 1_000);
        credited.cited = true;
        credited.cited_outlet = Some("apa".to_string());
        let plain = item("c2", "Yanğın söndürülüb - FOTO", 1_100);
        store.upsert_items(id, &[credited, plain], 1_100).unwrap();

        let (items, _) = store.window_data(0, 2_000, true).unwrap();
        let credited = items
            .iter()
            .find(|row| row.title == "Yanğın söndürülüb")
            .unwrap();
        assert!(credited.cited);
        assert_eq!(credited.cited_outlet.as_deref(), Some("apa"));

        let plain = items
            .iter()
            .find(|row| row.title == "Yanğın söndürülüb - FOTO")
            .unwrap();
        assert!(!plain.cited);
        assert_eq!(
            plain.cited_outlet, None,
            "original reporting credits nobody"
        );
    }

    /// A database written by a version without `cited_outlet`. `CREATE TABLE IF NOT EXISTS`
    /// leaves an existing table alone, so the column can only arrive by migration — and the rows
    /// already in the table must survive it.
    #[test]
    fn a_database_from_before_cited_outlet_gains_the_column_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE outlets (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, key TEXT NOT NULL);
                 CREATE TABLE sources (
                   id INTEGER PRIMARY KEY, outlet_id INTEGER NOT NULL REFERENCES outlets(id),
                   kind TEXT NOT NULL, name TEXT NOT NULL, locator TEXT NOT NULL,
                   enabled INTEGER NOT NULL DEFAULT 1, UNIQUE(kind, locator));
                 CREATE TABLE items (
                   id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL REFERENCES sources(id),
                   outlet_id INTEGER NOT NULL REFERENCES outlets(id), external_id TEXT NOT NULL,
                   url TEXT NOT NULL, title TEXT NOT NULL, description TEXT, section TEXT,
                   published_at INTEGER NOT NULL, first_seen INTEGER NOT NULL,
                   last_seen INTEGER NOT NULL, views INTEGER, cited INTEGER NOT NULL DEFAULT 0,
                   is_backfill INTEGER NOT NULL DEFAULT 0, UNIQUE(source_id, external_id));
                 INSERT INTO outlets (id, name, key) VALUES (1, 'APA', 'apa');
                 INSERT INTO sources (id, outlet_id, kind, name, locator, enabled)
                   VALUES (1, 1, 'rss', 'APA RSS', 'https://apa.az/rss', 1);
                 INSERT INTO items (id, source_id, outlet_id, external_id, url, title,
                                    published_at, first_seen, last_seen, cited)
                   VALUES (1, 1, 1, 'x', 'https://apa.az/x', 'Köhnə sətir', 100, 100, 100, 1);",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let (items, _) = store.window_data(0, 1_000, true).unwrap();
        assert_eq!(
            items.len(),
            1,
            "the row written before the migration survives"
        );
        assert!(items[0].cited, "its flags are untouched");
        assert_eq!(
            items[0].cited_outlet, None,
            "an origin that was never recorded stays unknown, and is read as such"
        );

        // Opening again must not try to add the column twice.
        let again = Store::open(&path).unwrap();
        assert_eq!(again.window_data(0, 1_000, true).unwrap().0.len(), 1);
    }

    /// The same for a table rather than a column: `meta` arrives on an existing database when it
    /// is next opened, and the rows already there are untouched.
    #[test]
    fn a_database_from_before_the_meta_table_gains_it_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let mut store = Store::open(&path).unwrap();
            let source_id = store
                .ensure_source(&spec("APA", "APA RSS", "https://apa.az/rss"), true)
                .unwrap();
            store
                .upsert_items(
                    source_id,
                    &[item("a", "Bakıda yollar bağlıdır", 1_000)],
                    1_000,
                )
                .unwrap();
            // A database from a version that had no `meta` table at all.
            store.conn.execute("DROP TABLE meta", []).unwrap();
            assert!(store.meta("anything").is_err(), "the table is gone");
        }

        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store.meta("anything").unwrap(),
            None,
            "opening recreates the table, and an unrecorded key is absent rather than an error"
        );
        store.set_meta("anything", "1").unwrap();
        assert_eq!(store.meta("anything").unwrap().as_deref(), Some("1"));
        store.set_meta("anything", "2").unwrap();
        assert_eq!(
            store.meta("anything").unwrap().as_deref(),
            Some("2"),
            "a repeat writes one value, not two"
        );
        assert_eq!(store.item_count().unwrap(), 1, "the rows survived");
    }
}
