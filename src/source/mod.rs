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
pub fn section_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = rest.split_once('/').map(|(_, r)| r)?;
    let segment = path.split(['/', '?', '#']).next()?;
    if segment.is_empty() {
        None
    } else {
        Some(segment.to_ascii_lowercase())
    }
}

const CITATION_MARKERS: &[&str] = &[
    "istinaden", "istinadla", "melumatina gore", "сообщает", "передает", "ссылаясь", "по данным",
];

/// True when the text credits another outlet. Such an item weighs half in coverage.
pub fn is_cited(text: &str) -> bool {
    let folded = text::fold(text);
    CITATION_MARKERS.iter().any(|marker| folded.contains(marker))
}
