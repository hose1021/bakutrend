//! RSS and Atom parsing through `feed-rs`, mapped onto `ParsedItem`.

use crate::error::ParseError;
use crate::source::{ParseOutcome, ParsedItem, is_cited, section_from_url};
use crate::text::{collapse_ws, decode_entities};

/// Parse feed bytes. Items without a link, title or timestamp are skipped and counted.
pub fn parse(bytes: &[u8]) -> Result<ParseOutcome, ParseError> {
    let feed = feed_rs::parser::parse(bytes).map_err(|e| ParseError::Feed(e.to_string()))?;
    let mut outcome = ParseOutcome::default();
    for entry in feed.entries {
        let Some(url) = entry.links.first().map(|link| link.href.clone()) else {
            outcome.skipped += 1;
            continue;
        };
        let title = entry
            .title
            .map(|t| collapse_ws(&decode_entities(&t.content)))
            .unwrap_or_default();
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
            .map(|t| collapse_ws(&decode_entities(&t.content)))
            .or_else(|| {
                entry
                    .content
                    .and_then(|c| c.body)
                    .map(|b| collapse_ws(&decode_entities(&b)))
            });
        outcome.items.push(ParsedItem {
            section: section_from_url(&url),
            cited: is_cited(description.as_deref().unwrap_or_default()),
            external_id: url.clone(),
            url,
            title,
            description,
            published_at,
            views: None,
            publisher: None,
        });
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("tests/fixtures/{name}")).expect("fixture present")
    }

    #[test]
    fn qafqazinfo_every_item_gets_a_distinct_identity_key() {
        // This feed repeats one guid on every item; keying on guid would collapse it to one.
        let out = parse(&fixture("qafqazinfo.rss.xml")).expect("parses");
        assert!(out.items.len() >= 50, "got {} items", out.items.len());
        let ids: std::collections::BTreeSet<_> = out.items.iter().map(|i| &i.external_id).collect();
        assert_eq!(ids.len(), out.items.len(), "identity keys must be unique");
    }

    #[test]
    fn azertag_items_parse_without_a_guid_element() {
        let out = parse(&fixture("azertag.rss.xml")).expect("parses");
        assert!(out.items.len() >= 20, "got {} items", out.items.len());
        assert!(out.items.iter().all(|i| !i.title.is_empty()));
        assert!(out.items.iter().all(|i| i.published_at > 1_600_000_000));
    }

    #[test]
    fn modern_az_titles_have_html_entities_decoded() {
        let out = parse(&fixture("modern.rss.xml")).expect("parses");
        assert!(out.items.len() >= 10);
        for item in &out.items {
            assert!(
                !item.title.contains("&nbsp;"),
                "undecoded entity in {:?}",
                item.title
            );
            assert!(
                !item.title.contains("&ldquo;"),
                "undecoded entity in {:?}",
                item.title
            );
        }
    }

    #[test]
    fn section_is_taken_from_the_url_path() {
        assert_eq!(
            section_from_url("https://apa.az/incident/x-995704").as_deref(),
            Some("incident")
        );
        assert_eq!(
            section_from_url("https://www.qafqazinfo.az/news/detail/x-521389").as_deref(),
            Some("news")
        );
        assert_eq!(section_from_url("https://azertag.az/").as_deref(), None);
    }

    #[test]
    fn is_cited_detects_agency_attribution() {
        assert!(is_cited(
            "“Qafqazinfo” APA-ya istinadən xəbər verir ki, hadisə olub"
        ));
        assert!(is_cited("TASS-a istinadla məlumat yayılıb"));
        assert!(!is_cited("Bakıda bu yollar bağlıdır"));
    }

    #[test]
    fn malformed_bytes_return_an_error_not_a_panic() {
        let err = parse(b"<rss><channel><item>").unwrap_err();
        assert!(matches!(err, crate::error::ParseError::Feed(_)));
    }
}
