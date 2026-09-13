//! Google News RSS, used only to seed the week window on an empty database.
//! Titles arrive as `Headline - Publisher`, and the RSS `<source>` element names the publisher.

use crate::error::ParseError;
use crate::source::{is_cited, section_from_url, ParseOutcome, ParsedItem};
use crate::text::{collapse_ws, decode_entities};

pub fn parse(bytes: &[u8]) -> Result<ParseOutcome, ParseError> {
    let feed = feed_rs::parser::parse(bytes).map_err(|e| ParseError::Feed(e.to_string()))?;
    // `feed-rs` ignores the RSS `<source>` element, so the publisher names come from the raw XML.
    let publishers = item_publishers(&String::from_utf8_lossy(bytes));
    let mut outcome = ParseOutcome::default();
    for (index, entry) in feed.entries.into_iter().enumerate() {
        let Some(url) = entry.links.first().map(|link| link.href.clone()) else {
            outcome.skipped += 1;
            continue;
        };
        let raw_title = entry
            .title
            .map(|t| collapse_ws(&decode_entities(&t.content)))
            .unwrap_or_default();
        let publisher = publishers
            .get(index)
            .map(|name| collapse_ws(name))
            .filter(|name| !name.is_empty());
        let title = strip_publisher_suffix(&raw_title, publisher.as_deref());
        if title.is_empty() {
            outcome.skipped += 1;
            continue;
        }
        let Some(published_at) = entry.published.or(entry.updated).map(|d| d.timestamp()) else {
            outcome.skipped += 1;
            continue;
        };
        let description = entry
            .summary
            .map(|t| collapse_ws(&decode_entities(&t.content)));
        outcome.items.push(ParsedItem {
            external_id: url.clone(),
            section: section_from_url(&url),
            cited: is_cited(description.as_deref().unwrap_or_default()),
            url,
            title,
            description,
            published_at,
            views: None,
            publisher,
        });
    }
    Ok(outcome)
}

/// The `<source>` text of each item, in document order. `feed-rs` ignores the RSS `<source>`
/// element, and without it there is nothing to strip the title suffix against and the store
/// would credit the seed source instead of the real outlet. Google emits bare `<item>` tags and
/// escapes any nested markup, so pairing positionally with `feed.entries` is exact.
fn item_publishers(xml: &str) -> Vec<String> {
    let mut publishers = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<item>") {
        let block = &rest[start..];
        let Some(end) = block.find("</item>") else { break };
        publishers.push(element_text(&block[..end], "source").unwrap_or_default());
        rest = &rest[start + end..];
    }
    publishers
}

/// Inner text of the first `<tag ...>…</tag>` in `xml`, with entities decoded.
fn element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)? + open.len();
    let start = xml[start..].find('>')? + start + 1;
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(decode_entities(&xml[start..end]))
}

/// Remove a trailing ` - Publisher`. `<source>` is the ground truth, so try it first with the
/// separators Google emits. Only when there is no publisher to match do we guess: the tail must
/// be a single word of at most 24 characters, which is what a site name looks like.
fn strip_publisher_suffix(title: &str, publisher: Option<&str>) -> String {
    if let Some(publisher) = publisher {
        for separator in [" - ", " — ", " | "] {
            let suffix = format!("{separator}{publisher}");
            if let Some(head) = title.strip_suffix(&suffix) {
                return head.trim().to_string();
            }
        }
    }
    match title.rsplit_once(" - ") {
        Some((head, tail))
            if !tail.contains(' ') && tail.chars().count() <= 24 && !head.is_empty() =>
        {
            head.trim().to_string()
        }
        _ => title.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn google_titles_lose_the_publisher_suffix() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item><title>Bakıda yollar bağlıdır - Day.Az</title>
        <link>https://news.google.com/rss/articles/abc</link>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate>
        <source url="https://www.day.az">Day.Az</source></item></channel></rss>"#;
        let out = parse(xml.as_bytes()).expect("parses");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].title, "Bakıda yollar bağlıdır");
        assert_eq!(out.items[0].publisher.as_deref(), Some("Day.Az"));
    }

    #[test]
    fn a_title_without_a_suffix_is_left_alone() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item><title>Sadə başlıq</title><link>https://news.google.com/rss/articles/x</link>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate></item></channel></rss>"#;
        let out = parse(xml.as_bytes()).expect("parses");
        assert_eq!(out.items[0].title, "Sadə başlıq");
        assert_eq!(out.items[0].publisher, None);
    }
}
