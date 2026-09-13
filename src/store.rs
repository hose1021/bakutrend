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

    /// Resolve an outlet by name, creating it when absent. Reuses an existing outlet
    /// whose key is a prefix, so Google's `Report.az` lands on the configured `Report`.
    pub fn resolve_outlet(&self, name: &str) -> Result<i64, StoreError> {
        let key = outlet_key(name);
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM outlets WHERE key = ?1
                 UNION ALL
                 SELECT id FROM outlets WHERE length(key) >= 4 AND (?1 LIKE key || '%' OR key LIKE ?1 || '%')
                 LIMIT 1",
                [&key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO outlets (name, key) VALUES (?1, ?2)",
            rusqlite::params![name, key],
        )?;
        Ok(self.conn.last_insert_rowid())
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
                   last_seen = excluded.last_seen,
                   title = excluded.title,
                   description = excluded.description,
                   section = excluded.section,
                   views = COALESCE(excluded.views, items.views),
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

fn resolve_outlet_in(tx: &rusqlite::Transaction<'_>, name: &str) -> Result<i64, StoreError> {
    let key = outlet_key(name);
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM outlets WHERE key = ?1
             UNION ALL
             SELECT id FROM outlets WHERE length(key) >= 4 AND (?1 LIKE key || '%' OR key LIKE ?1 || '%')
             LIMIT 1",
            [&key],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    tx.execute("INSERT INTO outlets (name, key) VALUES (?1, ?2)", rusqlite::params![name, key])?;
    Ok(tx.last_insert_rowid())
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
