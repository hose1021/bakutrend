//! Text normalization shared by story grouping, citation detection and the local filter.

use std::collections::BTreeSet;

/// Lowercase and strip Azerbaijani diacritics so `Bakı` and `baki` compare equal.
pub fn fold(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        // 'İ'.to_lowercase() yields "i" plus a combining dot, which would split a token.
        let lowered = match ch {
            'I' | 'İ' => 'i',
            _ => ch.to_lowercase().next().unwrap_or(ch),
        };
        out.push(match lowered {
            'ə' => 'e',
            'ı' => 'i',
            'ş' => 's',
            'ğ' => 'g',
            'ç' => 'c',
            'ö' => 'o',
            'ü' => 'u',
            'â' => 'a',
            'é' => 'e',
            'ё' => 'е',
            'й' => 'и',
            other => other,
        });
    }
    out
}

const NAMED_ENTITIES: &[(&str, &str)] = &[
    ("&nbsp;", " "),
    ("&ldquo;", "\u{201c}"),
    ("&rdquo;", "\u{201d}"),
    ("&laquo;", "\u{ab}"),
    ("&raquo;", "\u{bb}"),
    ("&mdash;", "\u{2014}"),
    ("&ndash;", "\u{2013}"),
    ("&hellip;", "\u{2026}"),
    ("&quot;", "\""),
    ("&apos;", "'"),
    ("&lt;", "<"),
    ("&gt;", ">"),
];

/// Decode HTML entities. `feed-rs` resolves predefined XML entities and character
/// references but retains unknown entities in escaped form, so `&nbsp;` arrives as text.
pub fn decode_entities(input: &str) -> String {
    let mut out = input.to_string();
    for (from, to) in NAMED_ENTITIES {
        out = out.replace(from, to);
    }
    // Numeric references, before the `&amp;` pass so `&#38;` is not rewritten twice.
    if out.contains("&#") {
        let mut decoded = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(start) = rest.find("&#") {
            decoded.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let (digits, tail) = match after.find(';') {
                Some(end) => (&after[..end], &after[end + 1..]),
                None => {
                    decoded.push_str(&rest[start..]);
                    rest = "";
                    break;
                }
            };
            let value = digits
                .strip_prefix(['x', 'X'])
                .map(|hex| u32::from_str_radix(hex, 16).ok())
                .unwrap_or_else(|| digits.parse::<u32>().ok());
            match value.and_then(char::from_u32) {
                Some(ch) => decoded.push(ch),
                None => {
                    decoded.push_str("&#");
                    decoded.push_str(digits);
                    decoded.push(';');
                }
            }
            rest = tail;
        }
        decoded.push_str(rest);
        out = decoded;
    }
    // `&amp;` last, so text that was escaped twice stays literal.
    out.replace("&amp;", "&")
}

/// Trim and collapse every run of whitespace to a single space.
pub fn collapse_ws(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

const STOPWORDS: &[&str] = &[
    "olan", "ucun", "daha", "olub", "deye", "barede", "sonra", "artiq", "hansi", "nece", "bele",
    "onun", "hemcinin", "bunu", "butun", "gore", "ile", "ile", "kimi", "ancaq", "lakin", "yeni",
    "этот", "который", "также", "было", "сообщает", "передает", "которая", "которые", "что",
    "для", "как", "они", "его", "уже", "будет", "может", "своих", "своей", "только", "очень",
];

/// Significant words of a headline: folded, split, at least 4 characters, no stopwords.
/// Sorted and deduplicated so callers can compare sets directly.
pub fn tokens(input: &str) -> Vec<String> {
    let folded = fold(input);
    let mut seen = BTreeSet::new();
    for word in folded.split(|c: char| !c.is_alphanumeric()) {
        if word.chars().count() < 4 || STOPWORDS.contains(&word) {
            continue;
        }
        seen.insert(word.to_string());
    }
    seen.into_iter().collect()
}

/// True when the folded text contains any folded keyword. Used by the local filter.
pub fn matches_any(input: &str, keywords: &[String]) -> bool {
    let folded = fold(input);
    keywords.iter().any(|k| {
        let needle = fold(k);
        !needle.is_empty() && folded.contains(&needle)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_strips_azerbaijani_diacritics_and_lowercases() {
        assert_eq!(fold("Bakıda GƏNCƏDƏ"), "bakida gencede");
        assert_eq!(fold("Şuşa, Qarabağ"), "susa, qarabag");
    }

    #[test]
    fn fold_handles_dotted_capital_i_without_combining_mark() {
        // 'İ'.to_lowercase() is "i̇" (i + U+0307); folding must not leave the combining dot.
        assert_eq!(fold("İSMAYILLI"), "ismayilli");
        assert_eq!(fold("İ").chars().count(), 1);
    }

    #[test]
    fn decode_entities_handles_html_entities_feed_rs_leaves_escaped() {
        assert_eq!(decode_entities("a&nbsp;b"), "a b");
        assert_eq!(decode_entities("&ldquo;Sitat&rdquo;"), "\u{201c}Sitat\u{201d}");
        assert_eq!(decode_entities("x &mdash; y"), "x \u{2014} y");
        assert_eq!(decode_entities("&#39;"), "'");
    }

    #[test]
    fn decode_entities_resolves_amp_last_so_escaped_entities_stay_literal() {
        // "&amp;nbsp;" means the literal text "&nbsp;", not a space.
        assert_eq!(decode_entities("&amp;nbsp;"), "&nbsp;");
    }

    #[test]
    fn collapse_ws_trims_and_squashes() {
        assert_eq!(collapse_ws("  Bakıda   bu\n\nyollar "), "Bakıda bu yollar");
    }

    #[test]
    fn tokens_drops_short_words_stopwords_and_duplicates_and_sorts() {
        assert_eq!(tokens("Bakıda bu yollar bağlıdır"), vec!["baglidir", "bakida", "yollar"]);
        assert!(tokens("və bu ki").is_empty());
    }

    #[test]
    fn matches_any_folds_both_sides() {
        let keywords = vec!["Bakı".to_string(), "Qarabağ".to_string()];
        assert!(matches_any("Gəncədə QARABAĞ yolu", &keywords));
        assert!(!matches_any("Tramp Zelenski ilə danışdı", &keywords));
    }
}
