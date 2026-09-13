//! Blocking HTTP fetching. The poller is sequential, so a client pool buys nothing;
//! a single client with a browser User-Agent is enough (`azertag.az` returns 400 without one).

use std::time::Duration;

use crate::error::FetchError;
use crate::source::{self, ParseOutcome, SourceKind};
use crate::store::SourceRow;

const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) bakutrend/0.1";

/// Anything that can turn a source row into parsed items. Tests supply a fixture-backed
/// implementation so the whole poll cycle runs offline.
pub trait Fetcher {
    fn fetch(&self, source: &SourceRow) -> Result<ParseOutcome, FetchError>;
}

pub fn rss_url(locator: &str) -> String {
    locator.to_string()
}

pub fn telegram_url(handle: &str) -> String {
    format!("https://t.me/s/{}", handle.trim_start_matches('@'))
}

pub fn google_url(query: &str, when: &str) -> String {
    let encoded = query.replace(' ', "+");
    format!("https://news.google.com/rss/search?q={encoded}+when:{when}&hl=az&gl=AZ&ceid=AZ:az")
}

pub struct HttpFetcher {
    client: reqwest::blocking::Client,
    google_query: String,
}

impl HttpFetcher {
    pub fn new(google_query: &str) -> Result<Self, FetchError> {
        let url = "https://news.google.com/".to_string();
        let client = reqwest::blocking::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|source| FetchError::Network { url, source })?;
        Ok(Self { client, google_query: google_query.to_string() })
    }
}

impl Fetcher for HttpFetcher {
    fn fetch(&self, source: &SourceRow) -> Result<ParseOutcome, FetchError> {
        let url = match source.kind {
            SourceKind::Rss => rss_url(&source.locator),
            SourceKind::Telegram => telegram_url(&source.locator),
            SourceKind::Google => google_url(&self.google_query, "7d"),
        };
        let response = self
            .client
            .get(&url)
            .send()
            .map_err(|source| FetchError::Network { url: url.clone(), source })?;
        let status = response.status();
        if !status.is_success() {
            return Err(FetchError::Http { status: status.as_u16(), url });
        }
        let body = response
            .bytes()
            .map_err(|source| FetchError::Network { url: url.clone(), source })?;

        match source.kind {
            SourceKind::Rss => {
                source::rss::parse(&body).map_err(|source| FetchError::Parse { url, source })
            }
            SourceKind::Google => {
                source::google::parse(&body).map_err(|source| FetchError::Parse { url, source })
            }
            SourceKind::Telegram => {
                let html = String::from_utf8_lossy(&body);
                let outcome = source::telegram::parse(&html);
                // HTTP 200 with no posts means the channel disabled previews. Reporting that
                // as "0 new items" would hide a dead source.
                if outcome.items.is_empty() {
                    return Err(FetchError::EmptyPreview { handle: source.locator.clone() });
                }
                Ok(outcome)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_urls_strip_the_at_sign() {
        assert_eq!(telegram_url("@qafqazinfo"), "https://t.me/s/qafqazinfo");
        assert_eq!(telegram_url("qafqazinfo"), "https://t.me/s/qafqazinfo");
    }

    #[test]
    fn google_urls_encode_the_query_and_window() {
        let url = google_url("azerbaycan baki", "7d");
        assert!(url.contains("q=azerbaycan+baki+when:7d"), "{url}");
        assert!(url.contains("hl=az"), "{url}");
    }
}
