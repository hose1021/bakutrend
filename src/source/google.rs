//! Google News RSS, used only to seed the week window on an empty database.
//! Titles arrive as `Headline - Publisher`, and the RSS `<source>` element names the publisher.

use std::collections::BTreeMap;

use crate::error::ParseError;
use crate::source::{ParseOutcome, ParsedItem, is_cited, section_from_url};
use crate::text::{cited_outlet, collapse_ws, decode_entities};

pub fn parse(bytes: &[u8]) -> Result<ParseOutcome, ParseError> {
    let feed = feed_rs::parser::parse(bytes).map_err(|e| ParseError::Feed(e.to_string()))?;
    // `feed-rs` ignores the RSS `<source>` element, so the publisher names come from the raw XML.
    let publishers = item_publishers(&String::from_utf8_lossy(bytes));
    let mut outcome = ParseOutcome::default();
    for entry in feed.entries {
        let Some(url) = entry.links.first().map(|link| link.href.clone()) else {
            outcome.skipped += 1;
            continue;
        };
        let raw_title = entry
            .title
            .map(|t| collapse_ws(&decode_entities(&t.content)))
            .unwrap_or_default();
        let publisher = publishers
            .get(&url)
            .map(|name| collapse_ws(name))
            .filter(|name| !name.is_empty());
        // An item whose publisher cannot be resolved would fall back to the synthetic
        // `Google News` outlet in the store and count as a distinct outlet in coverage —
        // coverage counts distinct outlets, never sources. Drop it instead.
        let Some(publisher) = publisher else {
            outcome.skipped += 1;
            continue;
        };
        let title = strip_publisher_suffix(&raw_title, Some(&publisher));
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
            cited_outlet: cited_outlet(description.as_deref().unwrap_or_default()),
            url,
            title,
            description,
            published_at,
            views: None,
            publisher: Some(publisher),
        });
    }
    Ok(outcome)
}

/// Map each item's `<link>` to the text of that item's `<source>`. `feed-rs` ignores the RSS
/// `<source>` element, and without it there is nothing to strip the title suffix against and the
/// store would credit the seed source instead of the real outlet. Keying on the link rather than
/// the item's position keeps attribution exact whatever the scan sees — attributes, a missing
/// `<source>`, or a block with no matching entry — where a positional walk would credit one
/// story's publisher to another.
fn item_publishers(xml: &str) -> BTreeMap<String, String> {
    let mut publishers = BTreeMap::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<item") {
        let after_name = &rest[start + "<item".len()..];
        // `<items>` and the like are not items; a real tag continues with `>`, `/` or a space.
        if !after_name.starts_with(['>', '/']) && !after_name.starts_with(char::is_whitespace) {
            rest = after_name;
            continue;
        }
        let block = &rest[start..];
        let Some(end) = block.find("</item>") else {
            break;
        };
        let block = &block[..end];
        if let (Some(link), Some(source)) =
            (element_text(block, "link"), element_text(block, "source"))
        {
            publishers.insert(link, source);
        }
        rest = &rest[start + end..];
    }
    publishers
}

/// Inner text of the first `<tag ...>…</tag>` in `xml`: a CDATA wrapper and any nested markup are
/// dropped, then entities are decoded. The publisher name must come out as plain text, or one
/// outlet arrives in the store as two.
fn element_text(xml: &str, tag: &str) -> Option<String> {
    let open = xml.find(&format!("<{tag}"))?;
    let start = tag_end(xml, open)?;
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    let inner = xml[start..end].trim();
    let inner = inner
        .strip_prefix("<![CDATA[")
        .and_then(|text| text.strip_suffix("]]>"))
        .unwrap_or(inner);
    Some(decode_entities(&strip_tags(inner)).trim().to_string())
}

/// Drop `<…>` markup, keeping the text between the tags.
fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let Some(end) = tag_end(rest, open) else {
            return out;
        };
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Index just past the `>` closing the tag that starts at `open`. A `>` inside a quoted attribute
/// value belongs to the attribute, not the tag. `None` when the tag never closes, so callers can
/// drop the partial markup instead of letting it through as an outlet name.
fn tag_end(input: &str, open: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, ch) in input[open..].char_indices() {
        match (quote, ch) {
            (Some(open_quote), c) if c == open_quote => quote = None,
            (Some(_), _) => {}
            (None, c @ ('"' | '\'')) => quote = Some(c),
            (None, '>') => return Some(open + offset + 1),
            (None, _) => {}
        }
    }
    None
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
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate>
        <source url="https://www.day.az">Day.Az</source></item></channel></rss>"#;
        let out = parse(xml.as_bytes()).expect("parses");
        assert_eq!(out.items[0].title, "Sadə başlıq");
        assert_eq!(out.items[0].publisher.as_deref(), Some("Day.Az"));
    }

    /// Attribution is keyed on the item's own link, so one item missing a `<source>` cannot
    /// shift its neighbours onto each other's outlet.
    #[test]
    fn an_item_without_a_source_does_not_shift_its_neighbours() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item><title>Birinci - Day.Az</title><link>https://news.google.com/rss/articles/1</link>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate>
        <source url="https://www.day.az">Day.Az</source></item>
        <item><title>İkinci</title><link>https://news.google.com/rss/articles/2</link>
        <pubDate>Mon, 14 Sep 2026 10:00:00 GMT</pubDate></item>
        <item><title>Üçüncü - APA</title><link>https://news.google.com/rss/articles/3</link>
        <pubDate>Mon, 14 Sep 2026 09:00:00 GMT</pubDate>
        <source url="https://apa.az">APA</source></item></channel></rss>"#;
        // The unattributed middle item is dropped (C4), and it takes nothing with it:
        // both neighbours keep their own publishers.
        let out = parse(xml.as_bytes()).expect("parses");
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.skipped, 1);
        assert_eq!(out.items[0].publisher.as_deref(), Some("Day.Az"));
        assert_eq!(out.items[1].publisher.as_deref(), Some("APA"));
    }

    /// An item with no link is skipped and counted, and its `<source>` must not leak onto the
    /// next item, whose publisher comes from its own link.
    #[test]
    fn a_linkless_item_is_skipped_and_leaves_the_next_publisher_intact() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item><title>Linksiz - Day.Az</title>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate>
        <source url="https://www.day.az">Day.Az</source></item>
        <item><title>İkinci xəbər - APA</title><link>https://news.google.com/rss/articles/2</link>
        <pubDate>Mon, 14 Sep 2026 10:00:00 GMT</pubDate>
        <source url="https://apa.az">APA</source></item></channel></rss>"#;
        let out = parse(xml.as_bytes()).expect("parses");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.skipped, 1);
        assert_eq!(out.items[0].publisher.as_deref(), Some("APA"));
        assert_eq!(out.items[0].title, "İkinci xəbər");
    }

    #[test]
    fn items_carrying_attributes_still_parse() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item foo="1" bar="2"><title>Bakıda yollar bağlıdır - Day.Az</title>
        <link>https://news.google.com/rss/articles/abc</link>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate>
        <source url="https://www.day.az">Day.Az</source></item></channel></rss>"#;
        let out = parse(xml.as_bytes()).expect("parses");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.items[0].title, "Bakıda yollar bağlıdır");
        assert_eq!(out.items[0].publisher.as_deref(), Some("Day.Az"));
    }

    #[test]
    fn a_cdata_wrapped_publisher_is_plain_text() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item><title>Bakıda yollar bağlıdır - Day.Az</title>
        <link>https://news.google.com/rss/articles/abc</link>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate>
        <source url="https://www.day.az"><![CDATA[Day.Az]]></source></item></channel></rss>"#;
        let out = parse(xml.as_bytes()).expect("parses");
        assert_eq!(out.items[0].publisher.as_deref(), Some("Day.Az"));
    }

    /// A `>` inside a quoted attribute value belongs to the attribute. Closing the tag there would
    /// leave `b">Day.Az` as the publisher, and a corrupted name splits one outlet into two.
    #[test]
    fn a_tag_whose_attribute_contains_a_gt_does_not_leak_markup() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item><title>Bakıda yollar bağlıdır - Day.Az</title>
        <link>https://news.google.com/rss/articles/abc</link>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate>
        <source url="https://www.day.az"><b title="a > b">Day.Az</b></source></item></channel></rss>"#;
        let out = parse(xml.as_bytes()).expect("parses");
        assert_eq!(out.items[0].publisher.as_deref(), Some("Day.Az"));
        assert_eq!(out.items[0].title, "Bakıda yollar bağlıdır");
    }

    /// The tag never closes, so the text before it survives and the partial markup is dropped
    /// rather than emitted as an outlet name. Untagged text passes through untouched.
    #[test]
    fn an_unterminated_tag_drops_the_partial_markup() {
        assert_eq!(strip_tags("Day.Az<b"), "Day.Az");
        assert_eq!(strip_tags("Day.Az<b title=\"unclosed"), "Day.Az");
        // The `>` here is inside the quotes, and the tag never closes: neither may end it.
        assert_eq!(strip_tags("Day.Az<b title=\"a > b"), "Day.Az");
        assert_eq!(strip_tags("APA"), "APA");
        assert_eq!(strip_tags("Day &amp; Az"), "Day &amp; Az");
    }

    #[test]
    fn entities_in_a_publisher_are_decoded() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item><title>Bakıda yollar bağlıdır - Day &amp; Az</title>
        <link>https://news.google.com/rss/articles/abc</link>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate>
        <source url="https://www.day.az">Day &amp; Az</source></item></channel></rss>"#;
        let out = parse(xml.as_bytes()).expect("parses");
        assert_eq!(out.items[0].publisher.as_deref(), Some("Day & Az"));
        assert_eq!(out.items[0].title, "Bakıda yollar bağlıdır");
    }

    /// An item with no resolvable publisher cannot contribute to coverage, and crediting
    /// the synthetic `Google News` outlet is exactly what the central rule forbids.
    #[test]
    fn an_unattributed_item_is_skipped_rather_than_credited_to_the_seed() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>t</title>
        <item><title>Attributed - APA</title><link>https://news.google.com/rss/articles/1</link>
        <pubDate>Mon, 14 Sep 2026 11:00:00 GMT</pubDate>
        <source url="https://apa.az">APA</source></item>
        <item><title>Atsız başlıq</title><link>https://news.google.com/rss/articles/2</link>
        <pubDate>Mon, 14 Sep 2026 10:00:00 GMT</pubDate></item></channel></rss>"#;
        let out = parse(xml.as_bytes()).expect("parses");
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.skipped, 1);
        assert_eq!(out.items[0].publisher.as_deref(), Some("APA"));
    }
}
