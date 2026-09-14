//! RSS and Atom parsing through `feed-rs`, mapped onto `ParsedItem`.

use crate::error::ParseError;
use crate::source::{ParseOutcome, ParsedItem, citation_text, is_cited, section_from_url};
use crate::text::{cited_outlet, plain_text};

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
            .map(|t| plain_text(&t.content))
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
            .map(|t| plain_text(&t.content))
            .or_else(|| entry.content.and_then(|c| c.body).map(|b| plain_text(&b)));
        outcome.items.push(ParsedItem {
            section: section_from_url(&url),
            cited: is_cited(&citation_text(&title, description.as_deref())),
            cited_outlet: cited_outlet(&citation_text(&title, description.as_deref())),
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

    /// The shipped locator is `https://haqqin.az/rss.xml`: the same feed answers at `/rss`, and
    /// `/rss/` — with the trailing slash the list once carried — answers HTTP 418.
    #[test]
    fn haqqin_items_carry_a_link_a_timestamp_and_words_without_markup() {
        let out = parse(&fixture("haqqin.rss.xml")).expect("parses");
        assert_eq!(out.items.len(), 20);
        let ids: std::collections::BTreeSet<_> = out.items.iter().map(|i| &i.external_id).collect();
        assert_eq!(ids.len(), out.items.len(), "identity keys must be unique");
        for item in &out.items {
            assert!(!item.title.is_empty(), "{item:?}");
            assert!(item.url.starts_with("https://haqqin.az/"), "{:?}", item.url);
            assert!(item.published_at > 1_700_000_000, "{item:?}");
            // This feed wraps its body in a CDATA section that begins with an `<img>` tag.
            if let Some(body) = &item.description {
                assert!(!body.contains('<'), "markup reached the body: {body:?}");
            }
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

    /// One feed, two items: the first credits another outlet in its headline only, the second in
    /// its summary only. Both are citations — feeds put the attribution wherever the editor put
    /// it, and looking in one field alone would miss half of them.
    const ATTRIBUTION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>APA</title>
  <item>
    <title>Yanğın söndürülüb - APA-ya istinadən</title>
    <link>https://apa.az/incident/1</link>
    <pubDate>Sun, 13 Sep 2026 19:06:15 GMT</pubDate>
    <description>Hadisə yerində işlər davam edir.</description>
  </item>
  <item>
    <title>Yollar bağlandı</title>
    <link>https://apa.az/incident/2</link>
    <pubDate>Sun, 13 Sep 2026 19:16:15 GMT</pubDate>
    <description>“Report”a istinadla məlumat verilir ki, hərəkət dayanıb.</description>
  </item>
  <item>
    <title>Tədbir keçirildi</title>
    <link>https://apa.az/incident/3</link>
    <pubDate>Sun, 13 Sep 2026 19:26:15 GMT</pubDate>
    <description>APA-nın müxbiri hadisə yerindən məlumat verir.</description>
  </item>
</channel></rss>"#;

    #[test]
    fn a_citation_in_the_headline_alone_is_still_a_citation() {
        let out = parse(ATTRIBUTION.as_bytes()).unwrap();
        assert_eq!(out.items.len(), 3);
        assert!(
            out.items[0].cited,
            "the marker is in the headline, and the headline is part of the text"
        );
        assert_eq!(
            out.items[0].cited_outlet.as_deref(),
            Some("apa"),
            "and the outlet it names is read from there too"
        );
        assert!(out.items[1].cited, "the summary carries the other one");
        assert_eq!(out.items[1].cited_outlet.as_deref(), Some("report"));
        // A headline with no marker is not evidence of anything: it is the absence of evidence,
        // and this program must not call it original reporting.
        assert!(!out.items[2].cited);
        assert_eq!(out.items[2].cited_outlet, None);
    }

    /// The Qafqazinfo feed escapes its own CDATA sections, so the wrapper arrives as literal
    /// text and comes into view only after the entities are decoded. A live poll stored 101
    /// bodies that began `<![CDATA[`, and the details view printed them as the news.
    #[test]
    fn a_cdata_section_around_a_body_is_markup_and_not_text() {
        const WRAPPED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>Qafqazinfo</title>
  <item>
    <title>Məktəblərdə qayda dəyişdi</title>
    <link>https://qafqazinfo.az/news/1</link>
    <pubDate>Sun, 13 Sep 2026 19:06:15 GMT</pubDate>
    <description>&lt;![CDATA[“Qafqazinfo” xəbər verir ki, bu barədə qərar qəbul edilib.]]&gt;</description>
  </item>
  <item>
    <title><![CDATA[Mötərizəsiz başlıq]]></title>
    <link>https://qafqazinfo.az/news/2</link>
    <pubDate>Sun, 13 Sep 2026 19:16:15 GMT</pubDate>
    <description><![CDATA[Adi mətn.]]></description>
  </item>
</channel></rss>"#;
        let out = parse(WRAPPED.as_bytes()).unwrap();
        assert_eq!(
            out.items[0].description.as_deref(),
            Some("“Qafqazinfo” xəbər verir ki, bu barədə qərar qəbul edilib.")
        );
        // The other spelling of the same thing, and a title it never touches.
        assert_eq!(out.items[1].description.as_deref(), Some("Adi mətn."));
        assert_eq!(out.items[1].title, "Mötərizəsiz başlıq");
        for item in &out.items {
            let text = format!("{}{:?}", item.title, item.description);
            assert!(!text.contains("CDATA"), "{text}");
        }
    }
}
