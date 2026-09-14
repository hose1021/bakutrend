//! Text normalization shared by story grouping, citation detection and the text filter.

use std::collections::{BTreeMap, BTreeSet};

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

/// A feed field as a reader sees it: no CDATA section, no entities, no markup, no run of
/// whitespace.
///
/// The wrapper is markup around the text, and some publishers escape their own: the Qafqazinfo
/// feed ships `<description>&lt;![CDATA[Manşet…]]&gt;</description>`, so the section arrives as
/// literal text and comes into view only once the entities are decoded. It is stripped after
/// decoding for that reason, and the unescaped form is stripped by the same pass.
pub fn plain_text(input: &str) -> String {
    let decoded = decode_entities(input);
    let trimmed = decoded.trim();
    let text = trimmed
        .strip_prefix("<![CDATA[")
        .and_then(|rest| rest.strip_suffix("]]>"))
        .unwrap_or(trimmed);
    collapse_ws(&drop_markup(text))
}

/// Drop markup, leaving a space where a tag stood.
///
/// Feeds put markup in the fields a reader reads: Haqqin.az sends
/// `<p><img src="…" /></p>Президент США выступил…`, and a body printed as it arrives opens with a
/// tag instead of a sentence. The space matters where the tag stood between two words —
/// `başlayır<br>Ətraflı:` is two words and must not become one.
fn drop_markup(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        let Some(start) = rest.find('<') else {
            out.push_str(rest);
            return out;
        };
        let Some(end) = tag_end(&rest[start + 1..]) else {
            // An unterminated `<` closes nothing, so it is the publisher's text, not markup.
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        out.push(' ');
        rest = &rest[start + end + 2..];
    }
}

/// The offset of the `>` that closes the tag whose name starts this slice, or `None` when this
/// `<` starts no tag. A quote holds a `>` inside an attribute, and a `<` followed by anything
/// but a name, a closing slash, a comment or a declaration is a character: in `Artım <1%` the
/// publisher wrote "less than", and dropping from there would delete the rest of the sentence.
fn tag_end(after: &str) -> Option<usize> {
    let mut chars = after.char_indices();
    match chars.next() {
        Some((_, glyph)) if glyph.is_ascii_alphabetic() || matches!(glyph, '/' | '!' | '?') => {}
        _ => return None,
    }
    let mut quote: Option<char> = None;
    for (index, glyph) in chars {
        match quote {
            Some(open) if glyph == open => quote = None,
            Some(_) => {}
            None if matches!(glyph, '"' | '\'') => quote = Some(glyph),
            None if glyph == '>' => return Some(index),
            None => {}
        }
    }
    None
}

const STOPWORDS: &[&str] = &[
    "olan",
    "ucun",
    "daha",
    "olub",
    "deye",
    "barede",
    "sonra",
    "artiq",
    "hansi",
    "nece",
    "bele",
    "onun",
    "hemcinin",
    "bunu",
    "butun",
    "gore",
    "ile",
    "ile",
    "kimi",
    "ancaq",
    "lakin",
    "yeni",
    "этот",
    "который",
    "также",
    "было",
    "сообщает",
    "передает",
    "которая",
    "которые",
    "что",
    "для",
    "как",
    "они",
    "его",
    "уже",
    "будет",
    "может",
    "своих",
    "своей",
    "только",
    "очень",
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

    // Spec §9: a run of two or more consecutive capitalized words is a proper-noun
    // phrase and is kept as ONE extra token, detected before folding destroys the
    // capitalization. Additive only — the per-word tokens above are untouched, so
    // similarity between headlines sharing a phrase can only rise.
    let mut run: Vec<&str> = Vec::new();
    for word in input.split_whitespace() {
        let bare = word.trim_matches(|c: char| !c.is_alphanumeric());
        if bare.chars().next().is_some_and(char::is_uppercase) {
            run.push(bare);
        } else if run.len() >= 2 {
            seen.insert(fold(&run.join(" ")).replace(' ', "_"));
            run.clear();
        } else {
            run.clear();
        }
    }
    if run.len() >= 2 {
        seen.insert(fold(&run.join(" ")).replace(' ', "_"));
    }
    seen.into_iter().collect()
}

/// A place name carries no event on its own, so it is a weaker grouping signal than
/// a person or organization, which are what two stories about the same event share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntityKind {
    Person,
    Organization,
    Team,
    Country,
    City,
    Place,
    Other,
}

impl EntityKind {
    /// True for a place name that carries no event on its own (Bakı, Azərbaycan).
    pub fn is_location(self) -> bool {
        matches!(
            self,
            EntityKind::Country | EntityKind::City | EntityKind::Place
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub text: String,
    pub kind: EntityKind,
}

/// Gazetteer of names worth tracking, keyed by folded surface form. Azerbaijani,
/// Russian and English spellings of one name sit under one entry so a headline in any
/// feed language lands on the same entity. Keys must already be folded: `fold`
/// lowercases but keeps Cyrillic letters Cyrillic, so a Russian key matches only after
/// both sides go through `fold`.
const GAZETTEER: &[(&[&str], EntityKind)] = &[
    // Azerbaijan: cities and regions.
    (&["baki", "baku", "баку"], EntityKind::City),
    (&["sumqayit", "сумгаит"], EntityKind::City),
    (&["gence", "ganca", "gyandzha", "гянджа"], EntityKind::City),
    (&["mingecevir", "мингечевир"], EntityKind::City),
    (&["susa", "shusha", "шуша"], EntityKind::City),
    (&["xankendi", "khankendi", "степанакерт"], EntityKind::City),
    (&["lenkeran", "лянкяран"], EntityKind::City),
    (&["seki", "sheki", "шеки"], EntityKind::City),
    (&["naxcivan", "nakhchivan", "нахичевань"], EntityKind::City),
    (&["semkir", "шамкир"], EntityKind::City),
    (&["sirvan", "ширван"], EntityKind::City),
    (&["qebele", "кабала"], EntityKind::City),
    (&["zagatala", "закаталы"], EntityKind::City),
    (&["berde", "борда"], EntityKind::City),
    (&["agdam", "агдам"], EntityKind::City),
    (&["fuzuli", "физули"], EntityKind::City),
    (&["cebrayil", "джебраил"], EntityKind::City),
    (&["zengilan", "зангилан"], EntityKind::City),
    (&["qubadli", "кубатлы"], EntityKind::City),
    (&["kelbecer", "кельбаджар"], EntityKind::City),
    (&["quba"], EntityKind::City),
    (&["salyan", "сальян"], EntityKind::City),
    (&["qusar", "кусары"], EntityKind::City),
    (&["neftcala", "нефтчала"], EntityKind::City),
    (&["bilesuvar", "билесувар"], EntityKind::City),
    (&["saatli", "саатлы"], EntityKind::City),
    (&["terter", "тертер"], EntityKind::City),
    (&["goranboy", "горанбой"], EntityKind::City),
    (&["gedebey", "гедабек"], EntityKind::City),
    (&["naftalan", "нафталан"], EntityKind::City),
    // Countries seen in this feed's news.
    (
        &["azerbaycan", "azerbaijan", "азербайджан"],
        EntityKind::Country,
    ),
    (&["turkiye", "turkey", "турция"], EntityKind::Country),
    (&["rusiya", "russia", "россия"], EntityKind::Country),
    (&["abs", "usa", "сша"], EntityKind::Country),
    (&["ukrayna", "ukraine", "украина"], EntityKind::Country),
    (&["iran", "иран"], EntityKind::Country),
    (&["israil", "israel", "израиль"], EntityKind::Country),
    (&["fransa", "france", "франция"], EntityKind::Country),
    (&["almaniya", "germany", "германия"], EntityKind::Country),
    (&["ingiltere", "england", "англия"], EntityKind::Country),
    (&["cin", "kitay", "china", "китай"], EntityKind::Country),
    (&["ermenistan", "armenia", "армения"], EntityKind::Country),
    (&["gurcustan", "georgia", "грузия"], EntityKind::Country),
    // Organizations.
    (&["socar"], EntityKind::Organization),
    (&["azal"], EntityKind::Organization),
    (&["ady"], EntityKind::Organization),
    (&["uefa", "уефа"], EntityKind::Organization),
    (&["fifa", "фифа"], EntityKind::Organization),
    (&["nato", "нато"], EntityKind::Organization),
    (&["bmt", "оон"], EntityKind::Organization),
    (&["ai", "еи"], EntityKind::Organization),
    (&["din"], EntityKind::Organization),
    (&["xin"], EntityKind::Organization),
    (&["milli_meclis"], EntityKind::Organization),
    // Football teams.
    (&["qarabag", "karabakh", "карабах"], EntityKind::Place),
    (&["neftci", "нефтчи"], EntityKind::Team),
    (&["barselona", "barcelona", "барселона"], EntityKind::Team),
    (&["real_madrid", "реал_мадрид"], EntityKind::Team),
];
fn gazetteer_lookup(folded_word: &str) -> Option<(String, EntityKind)> {
    let hit = |word: &str| {
        GAZETTEER
            .iter()
            .find(|(forms, _)| forms.contains(&word))
            .map(|(forms, kind)| (forms[0].to_string(), *kind))
    };
    // Azerbaijani case endings survive folding (`Bakıda` → `bakida`), so a miss is
    // retried with one ending stripped, longest first. Only a gazetteer hit counts,
    // so a wrong strip can never invent an entity the text does not name.
    hit(folded_word).or_else(|| {
        [
            "daki", "deki", "dan", "den", "nin", "nun", "da", "de", "in", "un", "ya", "ye", "a",
            "e",
        ]
        .iter()
        .filter_map(|suffix| folded_word.strip_suffix(suffix))
        .filter(|stem| stem.chars().count() >= 3)
        .find_map(hit)
    })
}

/// Named entities in a headline: gazetteer matches plus capitalized proper-noun runs.
/// `text` is FOLDED (lowercase, no Azerbaijani diacritics, words joined by `_`), so a
/// caller can compare two headlines' entities directly. Sorted by `text`, deduplicated.
///
/// A single capitalized word that is NOT in the gazetteer is deliberately NOT an entity:
/// sentence-initial capitalization is indistinguishable from a proper name (`Sabah` is
/// both "tomorrow" and a person's name), so a lone capital carries no signal. Two or
/// more consecutive capitalized words do count, as kind `Other`. Never uppercase-fold
/// a token that is entirely non-alphabetic — such tokens are skipped outright.
pub fn entities(input: &str) -> Vec<Entity> {
    let mut found: BTreeMap<String, EntityKind> = BTreeMap::new();
    let words: Vec<&str> = input.split_whitespace().collect();

    // Gazetteer matches: capitalized words only, folded, one lookup per word.
    for word in &words {
        let bare = word.trim_matches(|c: char| !c.is_alphanumeric());
        if bare.chars().next().is_some_and(char::is_uppercase)
            && bare.chars().any(char::is_alphabetic)
            && let Some((text, kind)) = gazetteer_lookup(&fold(bare))
        {
            found.entry(text).or_insert(kind);
        }
    }

    // Proper-noun runs: detected on the raw text, before folding destroys case. A run
    // also ends after a word that carries trailing punctuation — `Bakı, Azərbaycan`
    // names two things, not one phrase.
    //
    // The trailing empty word terminates the last run, so a headline that ends on a name
    // (`danışdı İlham Əliyev`) yields the same entity as one that ends on punctuation
    // (`danışdı İlham Əliyev.`). Without it the run is still pending when the loop ends and
    // is dropped, and the entity depends on how the headline happens to be punctuated.
    let mut run: Vec<&str> = Vec::new();
    for word in words.iter().copied().chain(std::iter::once("")) {
        let bare = word.trim_matches(|c: char| !c.is_alphanumeric());
        let capitalized = bare.chars().next().is_some_and(char::is_uppercase)
            && bare.chars().any(char::is_alphabetic);
        if capitalized {
            run.push(bare);
        }
        if !capitalized || !word.ends_with(char::is_alphanumeric) {
            if run.len() >= 2 {
                let text = fold(&run.join(" ")).replace(' ', "_");
                found.entry(text).or_insert(EntityKind::Other);
            }
            run.clear();
        }
    }

    found
        .into_iter()
        .map(|(text, kind)| Entity { text, kind })
        .collect()
}

/// The citation markers, one token sequence each, so `istinadlar` is not `istinadla`.
///
/// This table is the only one in the program. Detection (`source::is_cited`) and attribution
/// (`cited_outlet`) read the same markers, and a second copy would let the two disagree: an item
/// flagged as a repeat whose credited outlet could never be found, or a credit that was never
/// noted as one. It lives here because `source` already depends on `text`, and the reverse would
/// invert that layering.
pub const CITATION_MARKERS: &[&[&str]] = &[
    &["istinaden"],
    &["istinadla"],
    &["melumatina", "gore"],
    &["сообщает"],
    &["передает"],
    &["ссылаясь"],
    &["по", "данным"],
];

/// The outlet a text credits, when a citation marker names one. `None` when no marker
/// is present or the tokens before it hold no usable name — never guess an outlet that
/// is not written in the text.
pub fn cited_outlet(input: &str) -> Option<String> {
    let folded = fold(input);
    let folded_tokens: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    // `fold` maps one character to one character, so the folded text splits into the same tokens
    // as the original, in the same order. The original tokens are kept for their case, which
    // folding destroys and which is the only thing separating an outlet from the word before it.
    let original_tokens: Vec<&str> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let (marker_start, marker_len) = CITATION_MARKERS
        .iter()
        .filter_map(|marker| {
            folded_tokens
                .windows(marker.len())
                .position(|run| run == *marker)
                .map(|position| (position, marker.len()))
        })
        .min_by_key(|(position, _)| *position)?;

    // A case suffix may hang between the name and the marker: `APA-ya istinadən` folds
    // to tokens [.., apa, ya, istinaden]. The suffix is its own token, so it is skipped
    // and the last whole token before it is the bare folded name; quotes were already
    // dropped by the alphanumeric split.
    const CASE_SUFFIXES: &[&str] = &[
        "ya", "ye", "a", "e", "nin", "in", "nun", "un", "da", "de", "dan", "den",
    ];
    let mut end = marker_start;
    while end > 0 && CASE_SUFFIXES.contains(&folded_tokens[end - 1]) {
        end -= 1;
    }

    // A citation names its source on whichever side of the marker its language puts it:
    // Azerbaijani `APA-ya istinadən` puts the name before, Russian `сообщает Reuters` puts it
    // after. Whichever side has a capitalized word is the side that names the outlet.
    //
    // An outlet is a name, and a name is capitalized or quoted. Anything else is the sentence the
    // marker sits in: `как сообщает` and `тепла передает` are prose, and reading a word of it as a
    // source would turn an unattributed repeat into a citation that confirms nothing — the one
    // direction this classification must never move by accident.
    let indices = [end.checked_sub(1), Some(marker_start + marker_len)];
    for index in indices.into_iter().flatten() {
        let Some(name) = folded_tokens.get(index) else {
            continue;
        };
        // A capital at the start of a sentence says nothing: `Как сообщает Reuters` opens with a
        // capitalized function word. The stopword list is the cheapest guard against reading prose
        // as a name, and it is already here for tokenization.
        if name.chars().count() < 2 || STOPWORDS.contains(name) {
            continue;
        }
        let capitalized = original_tokens
            .get(index)
            .and_then(|token| token.chars().next())
            .is_some_and(char::is_uppercase);
        if capitalized {
            return Some(name.to_string());
        }
    }
    None
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
        assert_eq!(
            decode_entities("&ldquo;Sitat&rdquo;"),
            "\u{201c}Sitat\u{201d}"
        );
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

    /// A body arrives with the publisher's markup in it, and the card prints the body.
    #[test]
    fn plain_text_drops_markup_and_keeps_the_words() {
        assert_eq!(
            plain_text(
                r#"<p><img src="https://i.haqqin.az/x.jpg" width="190" border="0" /></p>Президент США выступил"#
            ),
            "Президент США выступил"
        );
        // A tag between two words is a boundary, not nothing.
        assert_eq!(plain_text("başlayır<br>Ətraflı:"), "başlayır Ətraflı:");
        // A `>` inside an attribute does not close the tag.
        assert_eq!(plain_text(r#"<a title="a>b">Mətn</a>"#), "Mətn");
        assert_eq!(plain_text("<![CDATA[Adi mətn.]]>"), "Adi mətn.");
    }

    /// A `<` that starts no tag is the publisher's own character, not markup.
    #[test]
    fn plain_text_keeps_a_less_than_that_starts_no_tag() {
        assert_eq!(plain_text("Artım &lt;1% olub"), "Artım <1% olub");
        assert_eq!(plain_text("5 < 6"), "5 < 6");
        // Nothing closes it, so nothing is dropped.
        assert_eq!(plain_text("Səhv <b başlıq"), "Səhv <b başlıq");
    }

    #[test]
    fn tokens_drops_short_words_stopwords_and_duplicates_and_sorts() {
        assert_eq!(
            tokens("Bakıda bu yollar bağlıdır"),
            vec!["baglidir", "bakida", "yollar"]
        );
        assert!(tokens("və bu ki").is_empty());
    }

    #[test]
    fn entities_finds_a_gazetteer_city_in_each_of_the_three_languages() {
        for headline in [
            "Bakıda avtomobil qəzası olub",
            "В Баку произошла авария",
            "Traffic accident reported in Baku",
        ] {
            let e = entities(headline);
            assert_eq!(
                e.first().map(|x| x.text.as_str()),
                Some("baki"),
                "all spellings must land on one entity: {headline}"
            );
            assert_eq!(e.first().map(|x| x.kind), Some(EntityKind::City));
        }
    }

    #[test]
    fn two_consecutive_capitalized_words_become_one_entity() {
        assert_eq!(
            entities("Donald Tramp gəlib çatıb"),
            vec![Entity {
                text: "donald_tramp".to_string(),
                kind: EntityKind::Other,
            }]
        );
    }

    /// A name at the very end of a headline is still a name. The run only ended when a
    /// non-capitalized word or trailing punctuation arrived, so a headline that finished on a
    /// capitalized run never emitted it and the entity depended on the punctuation.
    #[test]
    fn a_name_run_at_the_end_of_a_headline_is_kept() {
        let with_period = entities("danışdı İlham Əliyev.");
        let at_end = entities("danışdı İlham Əliyev");
        assert_eq!(
            at_end, with_period,
            "the trailing period must not decide the entity"
        );
        assert_eq!(
            at_end.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            vec!["ilham_eliyev"]
        );

        // Punctuation still separates two names, and the second one is at the end.
        assert_eq!(
            entities("İlham Əliyev, Mehriban Əliyeva")
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            vec!["ilham_eliyev", "mehriban_eliyeva"]
        );

        // A single capitalized word is still not a run, whatever it ends with.
        assert!(entities("Sabah yağış gözlənilir").is_empty());
        assert!(entities("Yağış gözlənilir Sabah").is_empty());
    }

    #[test]
    fn a_lone_non_gazetteer_capitalized_word_is_not_an_entity() {
        assert!(entities("Sabah yağış gözlənilir").is_empty());
    }

    #[test]
    fn entities_output_is_sorted_and_deduplicated() {
        let e = entities("Bakı və Qarabağ, Bakıda yenə");
        let texts: Vec<&str> = e.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, vec!["baki", "qarabag"]);
        assert_eq!(e[0].kind, EntityKind::City);
        assert_eq!(e[1].kind, EntityKind::Place);
    }

    #[test]
    fn is_location_is_true_for_places_and_false_for_actors() {
        assert!(EntityKind::City.is_location());
        assert!(EntityKind::Country.is_location());
        assert!(EntityKind::Place.is_location());
        assert!(!EntityKind::Person.is_location());
        assert!(!EntityKind::Organization.is_location());
        assert!(!EntityKind::Team.is_location());
    }

    #[test]
    fn cited_outlet_reads_the_name_before_the_marker() {
        assert_eq!(
            cited_outlet("“Qafqazinfo” APA-ya istinadən xəbər verir ki, hadisə olub"),
            Some("apa".to_string())
        );
        assert_eq!(
            cited_outlet("TASS-a istinadla məlumat yayılıb"),
            Some("tass".to_string())
        );
    }

    #[test]
    fn cited_outlet_is_none_without_a_marker() {
        assert_eq!(cited_outlet("Bakıda bu yollar bağlıdır"), None);
        assert_eq!(cited_outlet("İstinadlar göstərilib"), None);
    }

    #[test]
    fn cited_outlet_rejects_lowercase_prose_before_a_marker() {
        // Found on real data: Russian function words were being read as outlet names, which
        // turned an unattributed repeat into a citation crediting nobody in particular.
        assert_eq!(
            cited_outlet("как сообщает источник, погода испортится"),
            None
        );
        assert_eq!(cited_outlet("тепла передает агентство"), None);
        // The real name is capitalized, and still comes back.
        assert_eq!(
            cited_outlet("как сообщает Reuters о ситуации"),
            Some("reuters".to_string())
        );
        // A capitalized sentence opener is still prose: only the name after the marker counts.
        assert_eq!(
            cited_outlet("Как сообщает Reuters о ситуации"),
            Some("reuters".to_string())
        );
    }

    #[test]
    fn tokens_keeps_a_proper_noun_run_as_one_phrase_token() {
        let t = tokens("Milli Məclis iclas keçirdi");
        assert!(
            t.contains(&"milli_meclis".to_string()),
            "the run must survive as one token: {t:?}"
        );
        assert!(t.contains(&"milli".to_string()));
        assert!(t.contains(&"meclis".to_string()));
        // A single capitalized word is not a run: existing tokens are unchanged.
        assert_eq!(
            tokens("Bakıda bu yollar bağlıdır"),
            vec!["baglidir", "bakida", "yollar"]
        );
    }
}
