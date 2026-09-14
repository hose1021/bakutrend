//! Source types, item shape, and the text rules that apply to every ingested item.

pub mod google;
pub mod http;
pub mod rss;
pub mod telegram;

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
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let path = rest.split_once('/').map(|(_, r)| r)?;
    let segment = path.split('/').next()?;
    if segment.is_empty() {
        None
    } else {
        Some(segment.to_ascii_lowercase())
    }
}

/// Each marker is its own token sequence, so `istinadlar` is not `istinadla`.
const CITATION_MARKERS: &[&[&str]] = &[
    &["istinaden"],
    &["istinadla"],
    &["melumatina", "gore"],
    &["сообщает"],
    &["передает"],
    &["ссылаясь"],
    &["по", "данным"],
];

/// True when the text credits another outlet. Such an item weighs half in coverage.
pub fn is_cited(text: &str) -> bool {
    let folded = text::fold(text);
    let words: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    CITATION_MARKERS
        .iter()
        .any(|marker| words.windows(marker.len()).any(|run| run == *marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_cited_matches_whole_tokens_only() {
        assert!(is_cited(
            "“Qafqazinfo” APA-ya istinadən xəbər verir ki, hadisə olub"
        ));
        assert!(is_cited("TASS-a istinadla məlumat yayılıb"));
        assert!(is_cited("Məlumatına görə, hadisə gecə baş verib"));
        assert!(!is_cited("Bakıda bu yollar bağlıdır"));
        assert!(!is_cited("İstinadlar göstərilib"));
        assert!(!is_cited("Он сообщается в отчёте"));
    }

    #[test]
    fn section_from_url_ignores_query_and_fragment() {
        assert_eq!(section_from_url("https://host?next=/foo"), None);
        assert_eq!(
            section_from_url("https://host/path?x=1").as_deref(),
            Some("path")
        );
        assert_eq!(
            section_from_url("https://host/path#frag").as_deref(),
            Some("path")
        );
        assert_eq!(section_from_url("https://host"), None);
        assert_eq!(section_from_url("https://azertag.az/"), None);
    }
}
