//! SQLite persistence. `items` and `view_samples` are the only sources of truth;
//! stories are derived in memory by `cluster` and `score`.

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
        Ok(self
            .conn
            .execute("DELETE FROM view_samples WHERE ts < ?1", [before])?)
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
        // Same item, five minutes later: inside the ten-minute throttle, so no new row.
        assert_eq!(store.sample_views(now + 300).unwrap(), 0);
        assert_eq!(store.sample_views(now + 900).unwrap(), 1);
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

    #[test]
    fn prune_removes_only_samples_older_than_the_cutoff() {
        let (mut store, id) = telegram_fixture();
        let now = 1_700_000_000;
        store
            .upsert_items(id, &[telegram_item("p1", 500, now - 300)], now)
            .unwrap();
        store.sample_views(now).unwrap();
        store.sample_views(now + 900).unwrap();

        assert_eq!(store.prune_samples(now + 600).unwrap(), 1);
        let remaining: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM view_samples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }
}
