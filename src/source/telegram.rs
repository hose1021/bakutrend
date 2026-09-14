//! Extraction from Telegram public channel previews (`https://t.me/s/<handle>`).
//!
//! Posts are HTML, not XML: message text contains inline `<b>`, `<i>` and `<tg-emoji>`
//! elements, and `<br/>` separates the headline from the body.

use scraper::{Html, Selector};

use crate::source::{is_cited, section_from_url, ParseOutcome, ParsedItem};
use crate::text::{collapse_ws, decode_entities};

/// Telegram abbreviates view counts: `2.65K`, `1.1K`, `1.2M`, or a plain number, and groups
/// thousands with a no-break space (`12 345`). A value that fails to parse would silently drop
/// the post's engagement signal, so every kind of whitespace is stripped, not just the ends.
pub fn parse_views(raw: &str) -> Option<i64> {
    let cleaned = raw.replace(',', ".");
    let cleaned = cleaned.split_whitespace().collect::<String>();
    if cleaned.is_empty() {
        return None;
    }
    let (digits, multiplier) = match cleaned.chars().last()? {
        'K' | 'k' => (&cleaned[..cleaned.len() - 1], 1_000.0),
        'M' | 'm' => (&cleaned[..cleaned.len() - 1], 1_000_000.0),
        _ => (cleaned.as_str(), 1.0),
    };
    digits
        .parse::<f64>()
        .ok()
        .map(|value| (value * multiplier).round() as i64)
}

pub fn parse(html: &str) -> ParseOutcome {
    // Static selectors: a failure here is a programming error, not a runtime condition.
    let post_selector = Selector::parse("div.tgme_widget_message").expect("static selector");
    // The post timestamp is the footer anchor's `<time>`; an earlier `<time datetime>` elsewhere
    // in the post (link preview, forwarded header) is not the post time.
    let time_selector = Selector::parse("a.tgme_widget_message_date time[datetime]")
        .expect("static selector");
    let views_selector = Selector::parse(".tgme_widget_message_views").expect("static selector");
    let text_selector = Selector::parse(".tgme_widget_message_text").expect("static selector");
    let link_selector = Selector::parse("a.tgme_widget_message_date").expect("static selector");

    let document = Html::parse_document(html);
    let mut outcome = ParseOutcome::default();

    for post in document.select(&post_selector) {
        let Some(post_id) = post.value().attr("data-post") else {
            outcome.skipped += 1;
            continue;
        };
        let Some(published_at) = post
            .select(&time_selector)
            .next()
            .and_then(|t| t.value().attr("datetime"))
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|dt| dt.timestamp())
        else {
            outcome.skipped += 1;
            continue;
        };
        let Some(text_node) = post.select(&text_selector).next() else {
            // Media-only post with no caption: nothing to group on.
            outcome.skipped += 1;
            continue;
        };
        let body = collapse_ws(&decode_entities(&text_node.text().collect::<String>()));
        if body.is_empty() {
            outcome.skipped += 1;
            continue;
        }
        // The headline is everything before the first line break.
        let head_html = text_node.inner_html();
        let head_raw = head_html.split("<br").next().unwrap_or_default();
        let title = collapse_ws(&decode_entities(
            &Html::parse_fragment(head_raw).root_element().text().collect::<String>(),
        ));
        let title = if title.is_empty() { body.clone() } else { title };

        let url = post
            .select(&link_selector)
            .next()
            .and_then(|a| a.value().attr("href"))
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://t.me/{}", post_id.replace('/', "/")));
        let views = post
            .select(&views_selector)
            .next()
            .and_then(|v| parse_views(&v.text().collect::<String>()));

        // The syndication rule is not limited to article feeds: a channel reposting another
        // outlet's story is exactly the case it targets.
        let cited = is_cited(&body);
        outcome.items.push(ParsedItem {
            external_id: post_id.to_string(),
            section: section_from_url(&url),
            url,
            title,
            description: Some(body),
            published_at,
            views,
            cited,
            publisher: None,
        });
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("tests/fixtures/{name}")).expect("fixture present")
    }

    #[test]
    fn parse_views_understands_the_suffixes_telegram_uses() {
        assert_eq!(parse_views("2.65K"), Some(2650));
        assert_eq!(parse_views("1.1K"), Some(1100));
        assert_eq!(parse_views("340"), Some(340));
        assert_eq!(parse_views("1.2M"), Some(1_200_000));
        assert_eq!(parse_views("12\u{a0}345"), Some(12_345));
        assert_eq!(parse_views("453 000"), Some(453_000));
        assert_eq!(parse_views(""), None);
    }

    #[test]
    fn qafqazinfo_posts_carry_id_time_views_and_a_title() {
        let out = parse(&fixture("qafqazinfo.tg.html"));
        assert!(out.items.len() >= 10, "got {} posts", out.items.len());
        let ids: std::collections::BTreeSet<_> = out.items.iter().map(|i| &i.external_id).collect();
        assert_eq!(ids.len(), out.items.len(), "post ids must be unique");
        for item in &out.items {
            assert!(item.url.starts_with("https://t.me/"), "url was {:?}", item.url);
            assert!(item.published_at > 1_600_000_000);
            assert!(item.views.is_some_and(|v| v >= 0));
            assert!(!item.title.is_empty(), "post {:?} had no title", item.external_id);
        }
    }

    /// A hand-written page, not a capture. These assert exact semantics, so they must not
    /// depend on whichever posts the live channels happened to serve at capture time.
    const INLINE: &str = r#"<html><body>
      <div class="tgme_widget_message" data-post="chan/1">
        <a class="tgme_widget_message_date" href="https://t.me/chan/1">
          <time datetime="2026-09-13T19:06:15+00:00"></time></a>
        <div class="tgme_widget_message_text js-message_text" dir="auto">
          <b>İsmayıllıda maşın aşıb yandı - Sürücü yaralandı<br/><br/></b>
          Ətraflı: <a href="https://example.az/news/detail/x-1">link</a></div>
        <span class="tgme_widget_message_views">2.65K</span>
      </div>
      <div class="tgme_widget_message" data-post="chan/2">
        <a class="tgme_widget_message_date" href="https://t.me/chan/2">
          <time datetime="2026-09-13T20:06:15+00:00"></time></a>
        <div class="tgme_widget_message_text js-message_text" dir="auto">
          <i class="emoji"><b>🇷🇺</b></i> <b>Peskov: müzakirələr davam edir</b><br/><br/>Bakıda görüş keçirildi.</div>
        <span class="tgme_widget_message_views">1.1K</span>
      </div>
    </body></html>"#;

    #[test]
    fn the_title_is_the_first_line_not_the_whole_post() {
        let out = parse(INLINE);
        assert_eq!(out.items[0].title, "İsmayıllıda maşın aşıb yandı - Sürücü yaralandı");
        assert!(out.items[0].description.as_deref().is_some_and(|d| d.contains("Ətraflı")));
    }

    #[test]
    fn an_emoji_prefix_does_not_swallow_the_headline() {
        let out = parse(INLINE);
        assert_eq!(out.items[1].title, "🇷🇺 Peskov: müzakirələr davam edir");
    }

    #[test]
    fn identity_time_and_views_come_from_the_post_attributes() {
        let out = parse(INLINE);
        assert_eq!(out.items[0].external_id, "chan/1");
        assert_eq!(out.items[0].url, "https://t.me/chan/1");
        assert_eq!(out.items[0].views, Some(2650));
        assert_eq!(out.items[1].views, Some(1100));
    }

    /// One post crediting another outlet, one that credits nobody. Channels repost each other
    /// constantly, and the syndication rule is not limited to article feeds, so the marker has
    /// to be read off the post body the parser already extracts.
    const INLINE_CITATION: &str = r#"<html><body>
      <div class="tgme_widget_message" data-post="chan/4">
        <a class="tgme_widget_message_date" href="https://t.me/chan/4">
          <time datetime="2026-09-13T19:06:15+00:00"></time></a>
        <div class="tgme_widget_message_text js-message_text" dir="auto">
          <b>Yanğın söndürülüb</b><br/><br/>“APA”ya istinadən xəbər verir ki, hadisə olub.</div>
        <span class="tgme_widget_message_views">340</span>
      </div>
      <div class="tgme_widget_message" data-post="chan/5">
        <a class="tgme_widget_message_date" href="https://t.me/chan/5">
          <time datetime="2026-09-13T20:06:15+00:00"></time></a>
        <div class="tgme_widget_message_text js-message_text" dir="auto">
          <b>Sabah hava necə olacaq</b><br/><br/>Bakıda yağış gözlənilir.</div>
        <span class="tgme_widget_message_views">1.1K</span>
      </div>
    </body></html>"#;

    #[test]
    fn a_post_that_credits_another_outlet_is_cited() {
        let out = parse(INLINE_CITATION);
        assert!(out.items[0].cited, "a repost must weigh half in coverage");
        assert!(!out.items[1].cited, "nothing here credits another outlet");
    }

    /// A post whose earlier markup also carries a `<time datetime>` — a link preview or a
    /// forwarded-message header does this. The post time is the one in the date anchor.
    const INLINE_EARLY_TIMESTAMP: &str = r#"<html><body>
      <div class="tgme_widget_message" data-post="chan/3">
        <div class="tgme_link_preview"><time datetime="1970-01-01T00:00:00+00:00"></time>preview</div>
        <div class="tgme_widget_message_text js-message_text" dir="auto">Başlıq<br/><br/>Gövdə mətni</div>
        <a class="tgme_widget_message_date" href="https://t.me/chan/3">
          <time datetime="2026-09-13T19:06:15+00:00"></time></a>
        <span class="tgme_widget_message_views">340</span>
      </div>
    </body></html>"#;

    #[test]
    fn the_timestamp_is_the_footer_anchor_not_an_earlier_element() {
        let out = parse(INLINE_EARLY_TIMESTAMP);
        assert_eq!(out.items.len(), 1, "got {} posts", out.items.len());
        assert_eq!(out.items[0].published_at, 1_789_326_375);
    }

    #[test]
    fn a_page_with_no_post_containers_yields_no_items_and_no_skips() {
        let out = parse("<html><body><div class=\"tgme_channel_info\">previews off</div></body></html>");
        assert!(out.items.is_empty());
        assert_eq!(out.skipped, 0);
    }

    #[test]
    fn captured_pages_still_parse_with_titles_shorter_than_bodies() {
        // Non-brittle check on real bytes: the live feed picks the words, this test only
        // asserts the invariant that must hold whatever it published.
        let out = parse(&fixture("bakupost.tg.html"));
        assert!(out.items.len() >= 5, "got {} posts", out.items.len());
        for item in &out.items {
            assert!(!item.title.contains("<br"), "raw markup leaked into {:?}", item.title);
            assert!(
                item.description.as_deref().is_some_and(|d| d.len() >= item.title.len()),
                "title must not be longer than the body for post {:?}",
                item.external_id
            );
        }
    }
}
