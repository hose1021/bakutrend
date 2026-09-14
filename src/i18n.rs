//! Interface languages.
//!
//! Every word the screen shows lives here. The struct has no defaults and no optional fields,
//! so a language that misses a string does not compile: a half-translated screen is not a
//! state this program can be in.
//!
//! The news itself is never translated. Headlines arrive in whichever language their outlet
//! wrote them in, and this module only covers the chrome around them.

use crate::app::Sort;
use crate::score::{Provenance, VelocityBasis};
use crate::store::Window;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Az,
    Ru,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::En, Lang::Az, Lang::Ru];

    /// Parse a language code from the config or the command line. A region suffix is accepted,
    /// so the `az_AZ.UTF-8` shape of a locale variable is understood as well as plain `az`.
    pub fn parse(value: &str) -> Option<Self> {
        let code = value.trim().to_ascii_lowercase();
        let base = code.split(['_', '-', '.']).next().unwrap_or("");
        match base {
            "en" => Some(Self::En),
            "az" => Some(Self::Az),
            "ru" => Some(Self::Ru),
            _ => None,
        }
    }

    /// The next language in the cycle, so one key reaches all of them. The cycle is fixed
    /// rather than ordered by anything: a switcher that reorders itself is a switcher the user
    /// has to re-learn.
    pub fn next(self) -> Self {
        match self {
            Self::En => Self::Az,
            Self::Az => Self::Ru,
            Self::Ru => Self::En,
        }
    }

    /// The language's own name, written the way its speakers write it. This is the one line
    /// that must be readable to someone who cannot read the language they just left, which is
    /// exactly who reads it after a switch.
    pub fn name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Az => "Azərbaycan dili",
            Self::Ru => "Русский",
        }
    }

    pub fn strings(self) -> &'static Strings {
        match self {
            Self::En => &EN,
            Self::Az => &AZ,
            Self::Ru => &RU,
        }
    }

    /// The code the language is asked for by: `--lang`, `language` in the config, a `?lang=`
    /// link under a web page. `name` is what a reader is shown instead.
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Az => "az",
            Self::Ru => "ru",
        }
    }
}

pub struct Strings {
    // Header.
    /// `sources ok`, after the `19/20`.
    pub sources_ok: &'static str,
    pub sources_unknown: &'static str,
    pub polled: &'static str,
    /// What follows `polled` when no poll has finished yet.
    pub never: &'static str,
    /// What stands before the moment a static snapshot was built, preposition included: the
    /// sentence is `{snapshot_of} 2026-09-15 14:03 UTC`.
    pub snapshot_of: &'static str,
    /// The mark for a story that was not in the previous ranking at all. Written the way the
    /// language writes it, upper case included: Azerbaijani's capital `i` is `İ`, which no
    /// case mapping in this program can know.
    pub new_label: &'static str,
    pub story_one: &'static str,
    pub story_few: &'static str,
    pub story_many: &'static str,

    // Window tabs.
    pub window_hour: &'static str,
    pub window_day: &'static str,
    pub window_week: &'static str,
    /// The same windows inside a sentence: `the last hour`, `за последний час`.
    pub window_long_hour: &'static str,
    pub window_long_day: &'static str,
    pub window_long_week: &'static str,

    // List columns. The header row is upper case, so these are the words as they appear there.
    pub col_rank: &'static str,
    pub col_headline: &'static str,
    pub col_outlets: &'static str,
    pub col_status: &'static str,
    pub col_age: &'static str,
    /// The last column while the list is ordered by view growth, then by view count.
    pub col_growth: &'static str,
    pub col_views: &'static str,

    // The list orders. Short enough for a header, long enough to say what they sort on.
    pub sort_index: &'static str,
    pub sort_growth: &'static str,
    pub sort_coverage: &'static str,
    pub sort_views: &'static str,
    pub sort_chronology: &'static str,
    /// `sorted by {name}`.
    pub sorted: &'static str,

    // How a views-per-hour number was obtained, and how old it is.
    pub velocity_measured: &'static str,
    pub velocity_estimated: &'static str,
    pub velocity_stale: &'static str,
    pub velocity_none: &'static str,

    // A story's counts, in the three forms a counted noun can need.
    /// Which of those forms this language's counts actually take. Not every language uses all
    /// three, and which forms a number takes is the language's own rule rather than the
    /// struct's.
    pub plural_rule: fn(i64) -> Count,
    pub outlet_one: &'static str,
    pub outlet_few: &'static str,
    pub outlet_many: &'static str,
    pub view_one: &'static str,
    pub view_few: &'static str,
    pub view_many: &'static str,
    pub no_views: &'static str,

    // Provenance.
    pub provenance_independent: &'static str,
    pub provenance_citation: &'static str,
    pub provenance_repost: &'static str,

    // The card's blocks. Section headings are upper case; row labels are not.
    pub section_score: &'static str,
    pub section_signals: &'static str,
    pub section_sources: &'static str,
    /// The block that holds the story's own text, in the details view.
    pub section_content: &'static str,
    /// Shown when neither the source nor the story stored any text.
    pub body_missing: &'static str,
    pub coverage: &'static str,
    pub engagement: &'static str,
    pub freshness: &'static str,
    pub spread: &'static str,
    /// `+{count} in {span}`, `+{count} · son {span}`, `+{count} за {span}`.
    pub picked_up: &'static str,
    /// `none in {span}`, `0 · son {span}`, `нет за {span}`.
    pub none_picked_up: &'static str,
    /// `+{delta} since the previous {window} window` and the other two orders.
    pub moved_up: &'static str,
    pub moved_down: &'static str,
    pub moved_new: &'static str,

    // Durations: English glues the unit to the number (`12m`), Azerbaijani and Russian
    // separate them (`12 dəq`, `12 мин`).
    pub just_now: &'static str,
    pub unit_separator: &'static str,
    pub unit_minute: &'static str,
    pub unit_hour: &'static str,
    pub unit_day: &'static str,
    /// What follows the unit in a sentence but not in a column: ` ago`, ` əvvəl`, ` назад`.
    pub ago_suffix: &'static str,

    // States.
    pub empty_first_run: &'static str,
    /// `no stories in {window}`.
    pub no_stories_window: &'static str,
    /// The same sentence for the full-screen details view, where there is room for it.
    pub empty_window_long: &'static str,
    /// `1 story in {window}`: what the active window holds, whatever the filter is doing.
    pub thin_window: &'static str,
    /// `{shown} of {total} matching in {window}`.
    pub matching: &'static str,
    /// `nothing matches “{filter}” ({total} in {window})`.
    pub no_match: &'static str,
    /// `press w for {window}`.
    pub widen: &'static str,
    /// `sources failing`, before the count and the names.
    pub degraded: &'static str,
    /// Status prefixes. The error's own words stay as the system wrote them; only the thing
    /// this program says about them is translated.
    pub poll_failed: &'static str,
    pub database_read_failed: &'static str,
    pub source_list_failed: &'static str,
    pub keyboard_failed: &'static str,
    pub non_http_url: &'static str,
    pub browser_failed: &'static str,

    // The language switcher.
    /// The word for the key in the footer and the help.
    pub language: &'static str,
    /// `language: {name}` — the status after a switch, in the language just chosen.
    pub language_switched: &'static str,

    // Help overlay and footer. The footer is a row of keycaps: the key is styled apart from
    // what it does, so a reader can find the key without reading the sentence.
    pub help_title: &'static str,
    pub help_lines: &'static [&'static str],
    /// Always visible at the bottom of the overlay, so the scroll keys are on screen even when
    /// the lines that would explain them are below the fold.
    pub help_scroll_hint: &'static str,
    pub details_title: &'static str,
    /// The word the filter mode's footer leads with.
    pub filter_label: &'static str,
    pub footer_keys: &'static [Keycap],
    pub footer_ways_out: &'static [Keycap],
    pub footer_filter_keys: &'static [Keycap],
    pub footer_filter_ways_out: &'static [Keycap],
    pub footer_details_keys: &'static [Keycap],
    pub footer_details_ways_out: &'static [Keycap],
    pub footer_help_keys: &'static [Keycap],
    pub footer_help_ways_out: &'static [Keycap],
}

/// One footer entry: the key, and what it does.
pub type Keycap = (&'static str, &'static str);

impl Strings {
    pub fn window(&self, window: Window) -> &'static str {
        match window {
            Window::Hour => self.window_hour,
            Window::Day => self.window_day,
            Window::Week => self.window_week,
        }
    }

    /// The window as a phrase inside a sentence: `the last hour`, `за последний час`.
    pub fn window_long(&self, window: Window) -> &'static str {
        match window {
            Window::Hour => self.window_long_hour,
            Window::Day => self.window_long_day,
            Window::Week => self.window_long_week,
        }
    }

    /// What the list is ordered by.
    pub fn sort(&self, sort: Sort) -> &'static str {
        match sort {
            Sort::Index => self.sort_index,
            Sort::Growth => self.sort_growth,
            Sort::Coverage => self.sort_coverage,
            Sort::Views => self.sort_views,
            Sort::Chronology => self.sort_chronology,
        }
    }

    /// `sorted by prominence index`.
    pub fn sorted(&self, name: &str) -> String {
        fill(self.sorted, &[("name", name)])
    }

    /// The kind of number and how old it is, in one sentence: an estimate and a measurement are
    /// different claims, and a count nobody has refreshed is not today's pace.
    pub fn velocity_note(&self, basis: VelocityBasis, seconds: i64) -> String {
        let ago = self.ago(seconds);
        let template = match basis {
            VelocityBasis::Measured => self.velocity_measured,
            VelocityBasis::Estimated => self.velocity_estimated,
            VelocityBasis::Stale => self.velocity_stale,
        };
        fill(template, &[("ago", &ago)])
    }

    /// `1 story in the last hour`: what the active window holds. A count of one or two is not
    /// the same statement as "nothing happened", and the sentence says which it is.
    pub fn thin_window(&self, count: usize, window: Window) -> String {
        fill(
            self.thin_window,
            &[
                ("count", &self.stories(count)),
                ("window", self.window_long(window)),
            ],
        )
    }

    /// `no stories in the last hour`.
    pub fn empty_window(&self, window: Window) -> String {
        fill(
            self.no_stories_window,
            &[("window", self.window_long(window))],
        )
    }

    /// `2 of 14 matching in the last hour`.
    pub fn matching(&self, shown: usize, total: usize, window: &str) -> String {
        fill(
            self.matching,
            &[
                ("shown", &shown.to_string()),
                ("total", &total.to_string()),
                ("window", window),
            ],
        )
    }

    /// `nothing matches “külək” (14 in the last hour)`.
    pub fn no_match(&self, filter: &str, total: usize, window: &str) -> String {
        fill(
            self.no_match,
            &[
                ("filter", filter),
                ("total", &total.to_string()),
                ("window", window),
            ],
        )
    }

    /// `press w for the last 24 hours`.
    pub fn widen(&self, window: Window) -> String {
        fill(self.widen, &[("window", self.window_long(window))])
    }

    pub fn provenance(&self, provenance: Provenance) -> &'static str {
        match provenance {
            Provenance::Independent => self.provenance_independent,
            Provenance::Citation => self.provenance_citation,
            Provenance::Repost => self.provenance_repost,
        }
    }

    /// `19/20 sources ok`, or the honest admission that no poll has reported yet.
    pub fn health(&self, ok: Option<usize>, total: usize) -> String {
        match ok {
            Some(ok) => format!("{ok}/{total} {}", self.sources_ok),
            None => self.sources_unknown.to_string(),
        }
    }

    pub fn polled(&self, seconds_ago: i64) -> String {
        format!("{} {}", self.polled, self.ago_sentence(seconds_ago))
    }

    pub fn stories(&self, count: usize) -> String {
        let count = count as i64;
        format!(
            "{count} {}",
            self.counted(count, self.story_one, self.story_few, self.story_many)
        )
    }

    pub fn outlets(&self, count: i64) -> String {
        format!(
            "{count} {}",
            self.counted(count, self.outlet_one, self.outlet_few, self.outlet_many)
        )
    }

    pub fn views(&self, count: i64) -> String {
        format!(
            "{count} {}",
            self.counted(count, self.view_one, self.view_few, self.view_many)
        )
    }

    /// The form this language's rule picks for the count.
    fn counted<'a>(&self, count: i64, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
        match (self.plural_rule)(count) {
            Count::One => one,
            Count::Few => few,
            Count::Many => many,
        }
    }

    /// A duration for a column: the number and the unit, nothing else.
    pub fn ago(&self, seconds: i64) -> String {
        if seconds < 60 {
            return self.just_now.to_string();
        }
        let (count, unit) = self.split(seconds);
        format!("{count}{}{unit}", self.unit_separator)
    }

    /// The same duration as a sentence: `12m ago`, `12 dəq əvvəl`, `12 мин назад`.
    pub fn ago_sentence(&self, seconds: i64) -> String {
        if seconds < 60 {
            return self.just_now.to_string();
        }
        format!("{}{}", self.ago(seconds), self.ago_suffix)
    }

    /// A duration for a column. Younger than a minute is written as a bound rather than as a
    /// phrase: `just now` and `только что` are nine and ten columns wide, and a column that
    /// wide costs the headline its room while saying only what `<1m` already says.
    pub fn age(&self, seconds: i64) -> String {
        if seconds < 60 {
            return format!("<1{}{}", self.unit_separator, self.unit_minute);
        }
        self.ago(seconds)
    }

    /// How far the story moved since the previous window of the same length. `None` is a story
    /// that was not in that ranking at all, which is not the same as one that stayed put.
    pub fn moved(&self, delta: Option<i64>, window: &str) -> String {
        match delta {
            Some(movement) => {
                let template = if movement > 0 {
                    self.moved_up
                } else {
                    self.moved_down
                };
                fill(
                    template,
                    &[("delta", &movement.to_string()), ("window", window)],
                )
            }
            None => fill(self.moved_new, &[("window", window)]),
        }
    }

    /// The fastest post of a story, as `178/h ×1.2`.
    pub fn pace(&self, per_hour: f64, relative: Option<f64>) -> String {
        match relative {
            Some(multiple) if multiple >= 1.05 => format!("{per_hour:.0}/h ×{multiple:.1}"),
            _ => format!("{per_hour:.0}/h"),
        }
    }

    /// The story's best relative pace, for the growth column: a multiple of the channel's own
    /// measured pace, or a dash when nothing was measured. Never a percentage and never a share
    /// of readers: it says how far above its channel's normal pace a post ran.
    pub fn growth(&self, relative: Option<f64>) -> String {
        match relative {
            Some(multiple) => format!("×{multiple:.1}"),
            None => "—".to_string(),
        }
    }

    /// How many outlets picked the story up inside the recent sub-window, and how long that
    /// window is: `+2 in 4h`, `нет за 4 ч`.
    pub fn picked_up(&self, count: i64, span_seconds: i64) -> String {
        let span = self.ago(span_seconds);
        if count <= 0 {
            fill(self.none_picked_up, &[("span", &span)])
        } else {
            fill(
                self.picked_up,
                &[("count", &count.to_string()), ("span", &span)],
            )
        }
    }

    pub fn degraded_status(&self, count: usize, names: &str) -> String {
        format!("{}: {count} · {names}", self.degraded)
    }

    /// The confirmation a language switch leaves on screen, written in the language it just
    /// switched to.
    pub fn switched(&self, name: &str) -> String {
        fill(self.language_switched, &[("name", name)])
    }

    fn split(&self, seconds: i64) -> (i64, &'static str) {
        match seconds {
            60..=3599 => (seconds / 60, self.unit_minute),
            3600..=86_399 => (seconds / 3600, self.unit_hour),
            _ => (seconds / 86_400, self.unit_day),
        }
    }
}

/// Fill a `{name}` placeholder. Word order differs between languages, so a sentence travels as
/// one string instead of as pieces the caller glues together in English order.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in values {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// Which of a counted noun's three forms a count takes.
///
/// Languages disagree about this, and the disagreement is not cosmetic: Russian needs all three
/// forms, and getting it wrong is the difference between `2 издания` and `2 издание`; English
/// uses two (`21 stories`, not `21 story`), and Azerbaijani one, because a numeral already
/// carries the number and the noun does not agree with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    One,
    Few,
    Many,
}

/// English: the singular for exactly one, the plural for every other count. `21 stories`, not
/// `21 story`.
fn english_count(count: i64) -> Count {
    if count == 1 { Count::One } else { Count::Many }
}

/// Russian: the last two digits decide, which is why `21 издание` is a singular form.
fn slavic_count(count: i64) -> Count {
    let last_two = count % 100;
    let last = count % 10;
    if last == 1 && last_two != 11 {
        Count::One
    } else if (2..=4).contains(&last) && !(12..=14).contains(&last_two) {
        Count::Few
    } else {
        Count::Many
    }
}

/// Azerbaijani writes one form after every numeral, so the singular string is the only one its
/// counted nouns ever take.
fn turkic_count(_: i64) -> Count {
    Count::One
}

static EN: Strings = Strings {
    sources_ok: "sources ok",
    sources_unknown: "sources unknown",
    polled: "polled",
    never: "never",
    snapshot_of: "static snapshot of",
    new_label: "NEW",
    story_one: "story",
    story_few: "stories",
    story_many: "stories",
    plural_rule: english_count,
    window_hour: "1h",
    window_day: "24h",
    window_week: "7d",
    window_long_hour: "the last hour",
    window_long_day: "the last 24 hours",
    window_long_week: "the last week",
    col_rank: "#",
    col_headline: "HEADLINE",
    col_outlets: "OUTLETS",
    col_status: "STATUS",
    col_age: "AGE",
    col_growth: "GROWTH",
    col_views: "VIEWS",
    sort_index: "prominence index",
    sort_growth: "recent growth",
    sort_coverage: "coverage",
    sort_views: "views",
    sort_chronology: "newest first",
    sorted: "sorted by {name}",
    velocity_measured: "measured {ago}",
    velocity_estimated: "estimate, last sample {ago}",
    velocity_stale: "stale, last sample {ago}",
    velocity_none: "no pace measured",
    outlet_one: "outlet",
    outlet_few: "outlets",
    outlet_many: "outlets",
    view_one: "view",
    view_few: "views",
    view_many: "views",
    no_views: "no view count",
    provenance_independent: "no citation seen",
    provenance_citation: "citation",
    provenance_repost: "repost",
    section_score: "PROMINENCE INDEX",
    section_signals: "SIGNALS",
    section_sources: "SOURCES",
    section_content: "CONTENT",
    body_missing: "no text stored for this publication",
    coverage: "Coverage",
    engagement: "Engagement",
    freshness: "Freshness",
    spread: "Spread",
    picked_up: "+{count} in {span}",
    none_picked_up: "none in {span}",
    moved_up: "+{delta} since the previous {window} window",
    moved_down: "{delta} since the previous {window} window",
    moved_new: "new since the previous {window} window",
    just_now: "just now",
    unit_separator: "",
    unit_minute: "m",
    unit_hour: "h",
    unit_day: "d",
    ago_suffix: " ago",
    empty_first_run: "No stories yet: the first poll is still running. This screen fills in about a minute.",
    no_stories_window: "no stories in {window}",
    empty_window_long: "No stories in this window since the last poll.",
    thin_window: "{count} in {window}",
    matching: "{shown} of {total} matching in {window}",
    no_match: "nothing matches “{filter}” ({total} in {window})",
    widen: "press w for {window}",
    degraded: "sources failing",
    poll_failed: "poll failed",
    database_read_failed: "database read failed",
    source_list_failed: "source list read failed",
    keyboard_failed: "keyboard input failed",
    non_http_url: "refusing to open a non-http URL",
    browser_failed: "browser launch failed",
    language: "language",
    language_switched: "language: {name}",
    help_title: " keys ",
    help_lines: &[
        "1 / 2 / 3        switch window (1h, 24h, 7d)",
        "w, Tab           widen the window (1h, 24h, 7d)",
        "j / k, arrows    move selection",
        "g / G, Home/End  jump to top / bottom",
        "[ / ]            choose which source Enter opens",
        "Enter            open that source's article in the browser",
        "d                open the selected story on its own, scrolled",
        "s                change the sort order",
        "l                switch the language (English, Azərbaycan dili, Русский)",
        "/                filter by text: Backspace deletes, Enter keeps it, Esc clears it",
        "r                poll now",
        "?                close this help",
        "j / k in a view  scroll its lines",
        "Esc              leave what is open, else quit",
        "q                quit",
    ],
    help_scroll_hint: "j/k scroll · Esc closes",
    details_title: " story ",
    filter_label: "filter",
    footer_keys: &[
        ("1-3", "windows"),
        ("j/k", "move"),
        ("Enter", "open"),
        ("/", "filter"),
        ("l", "language"),
        ("s", "sort"),
        ("d", "details"),
        ("r", "poll"),
    ],
    footer_ways_out: &[("?", "help"), ("q", "quit")],
    footer_filter_keys: &[("Backspace", "deletes")],
    footer_filter_ways_out: &[("Enter", "keep"), ("Esc", "clear")],
    footer_details_keys: &[("j/k", "scroll"), ("[/]", "source")],
    footer_details_ways_out: &[("Esc", "back"), ("q", "quit")],
    footer_help_keys: &[("j/k", "scroll")],
    footer_help_ways_out: &[("Esc", "close"), ("q", "quit")],
};

static AZ: Strings = Strings {
    sources_ok: "mənbə işləyir",
    sources_unknown: "mənbələr məlum deyil",
    polled: "sorğu",
    never: "heç vaxt",
    snapshot_of: "statik surət:",
    new_label: "YENİ",
    story_one: "xəbər",
    story_few: "xəbər",
    story_many: "xəbər",
    plural_rule: turkic_count,
    window_hour: "1s",
    window_day: "24s",
    window_week: "7g",
    window_long_hour: "son saat",
    window_long_day: "son 24 saat",
    window_long_week: "son həftə",
    col_rank: "#",
    col_headline: "BAŞLIQ",
    col_outlets: "SAYT",
    col_status: "STATUS",
    col_age: "VAXT",
    col_growth: "ARTIM",
    col_views: "BAXIŞ",
    sort_index: "görünürlük indeksi",
    sort_growth: "son artım",
    sort_coverage: "əhatə",
    sort_views: "baxışlar",
    sort_chronology: "ən yenilər",
    sorted: "{name} üzrə sıralanıb",
    velocity_measured: "ölçülüb {ago}",
    velocity_estimated: "təxmin, son nümunə {ago}",
    velocity_stale: "köhnəlib, son nümunə {ago}",
    velocity_none: "ölçmə yoxdur",
    outlet_one: "sayt",
    outlet_few: "sayt",
    outlet_many: "sayt",
    view_one: "baxış",
    view_few: "baxış",
    view_many: "baxış",
    no_views: "baxış yoxdur",
    provenance_independent: "istinadsız",
    provenance_citation: "istinad",
    provenance_repost: "təkrar",
    section_score: "GÖRÜNÜRLÜK İNDEKSİ",
    section_signals: "GÖSTƏRİCİLƏR",
    section_sources: "MƏNBƏLƏR",
    section_content: "MƏZMUN",
    body_missing: "bu nəşr üçün mətn saxlanmayıb",
    coverage: "Əhatə",
    engagement: "Maraq",
    freshness: "Təzəlik",
    spread: "Yayılma",
    picked_up: "+{count} · {span}",
    none_picked_up: "0 · {span}",
    moved_up: "+{delta} · əvvəlki {window} aralığından bəri",
    moved_down: "{delta} · əvvəlki {window} aralığından bəri",
    moved_new: "yeni · əvvəlki {window} aralığında yox idi",
    just_now: "indi",
    unit_separator: " ",
    unit_minute: "dəq",
    unit_hour: "saat",
    unit_day: "gün",
    ago_suffix: " əvvəl",
    empty_first_run: "Hələ xəbər yoxdur: ilk sorğu davam edir. Bu ekran təxminən bir dəqiqəyə dolur.",
    no_stories_window: "{window}da xəbər yoxdur",
    empty_window_long: "Son sorğudan bəri bu aralıqda xəbər yoxdur.",
    thin_window: "{count} · {window}",
    matching: "{shown} / {total} uyğun · {window}",
    no_match: "“{filter}” üzrə heç nə tapılmadı ({total} · {window})",
    widen: "w düyməsi: {window}",
    degraded: "cavab verməyən mənbələr",
    poll_failed: "sorğu alınmadı",
    database_read_failed: "bazadan oxumaq alınmadı",
    source_list_failed: "mənbə siyahısı oxunmadı",
    keyboard_failed: "klaviatura girişi kəsildi",
    non_http_url: "http olmayan ünvan açılmır",
    browser_failed: "brauzer açılmadı",
    language: "dil",
    language_switched: "dil: {name}",
    help_title: " düymələr ",
    help_lines: &[
        "1 / 2 / 3        aralığı dəyiş (1 saat, 24 saat, 7 gün)",
        "w, Tab           aralığı genişləndir",
        "j / k, oxlar     seçimi hərəkət etdir",
        "g / G, Home/End  başa / sona keç",
        "[ / ]            Enter hansı saytı açacağını seç",
        "Enter            o saytın məqaləsini brauzerdə aç",
        "d                seçilmiş xəbəri ayrıca aç, sürüşdürməklə",
        "s                sıralama dəyiş",
        "l                dili dəyiş (English, Azərbaycan dili, Русский)",
        "/                mətnlə filtr: Backspace silir, Enter saxlayır, Esc təmizləyir",
        "r                indi sorğu göndər",
        "?                bu köməyi bağla",
        "j / k            açıq pəncərədə sətirləri sürüşdür",
        "Esc              açıq olanı bağla, yoxdursa çıx",
        "q                çıx",
    ],
    help_scroll_hint: "j/k sürüşdür · Esc bağlayır",
    details_title: " xəbər ",
    filter_label: "filtr",
    footer_keys: &[
        ("1-3", "aralıq"),
        ("j/k", "hərəkət"),
        ("Enter", "aç"),
        ("/", "filtr"),
        ("l", "dil"),
        ("s", "sıra"),
        ("d", "ətraflı"),
        ("r", "sorğu"),
    ],
    footer_ways_out: &[("?", "kömək"), ("q", "çıx")],
    footer_filter_keys: &[("Backspace", "silir")],
    footer_filter_ways_out: &[("Enter", "saxla"), ("Esc", "təmizlə")],
    footer_details_keys: &[("j/k", "sürüşdür"), ("[/]", "sayt")],
    footer_details_ways_out: &[("Esc", "geri"), ("q", "çıx")],
    footer_help_keys: &[("j/k", "sürüşdür")],
    footer_help_ways_out: &[("Esc", "bağla"), ("q", "çıx")],
};

static RU: Strings = Strings {
    sources_ok: "источников отвечают",
    sources_unknown: "источники неизвестны",
    polled: "опрос",
    never: "никогда",
    snapshot_of: "статический снимок от",
    new_label: "НОВО",
    story_one: "новость",
    story_few: "новости",
    story_many: "новостей",
    plural_rule: slavic_count,
    window_hour: "1ч",
    window_day: "24ч",
    window_week: "7д",
    window_long_hour: "за последний час",
    window_long_day: "за последние 24 часа",
    window_long_week: "за последнюю неделю",
    col_rank: "#",
    col_headline: "ЗАГОЛОВОК",
    col_outlets: "ИСТ.",
    col_status: "СТАТУС",
    col_age: "ВОЗРАСТ",
    col_growth: "РОСТ",
    col_views: "ПРОСМ.",
    sort_index: "индекс заметности",
    sort_growth: "недавний рост",
    sort_coverage: "широта освещения",
    sort_views: "просмотры",
    sort_chronology: "сначала новые",
    sorted: "сортировка: {name}",
    velocity_measured: "измерено {ago}",
    velocity_estimated: "оценка, последний замер {ago}",
    velocity_stale: "устарело, последний замер {ago}",
    velocity_none: "скорость не измерена",
    outlet_one: "издание",
    outlet_few: "издания",
    outlet_many: "изданий",
    view_one: "просмотр",
    view_few: "просмотра",
    view_many: "просмотров",
    no_views: "просмотров нет",
    provenance_independent: "ссылки нет",
    provenance_citation: "цитата",
    provenance_repost: "репост",
    section_score: "ИНДЕКС ЗАМЕТНОСТИ",
    section_signals: "ПОКАЗАТЕЛИ",
    section_sources: "ИСТОЧНИКИ",
    section_content: "СОДЕРЖАНИЕ",
    body_missing: "текст этой публикации не сохранён",
    coverage: "Охват",
    engagement: "Интерес",
    freshness: "Свежесть",
    spread: "Подхваты",
    picked_up: "+{count} за {span}",
    none_picked_up: "нет за {span}",
    moved_up: "+{delta} с прошлого окна {window}",
    moved_down: "{delta} с прошлого окна {window}",
    moved_new: "новое с прошлого окна {window}",
    just_now: "только что",
    unit_separator: " ",
    unit_minute: "мин",
    unit_hour: "ч",
    unit_day: "дн",
    ago_suffix: " назад",
    empty_first_run: "Новостей пока нет: первый опрос ещё идёт. Экран заполнится примерно через минуту.",
    no_stories_window: "{window} новостей нет",
    empty_window_long: "С последнего опроса в этом окне новостей нет.",
    thin_window: "{count} {window}",
    matching: "{shown} из {total} · {window}",
    no_match: "по запросу «{filter}» ничего нет ({total} {window})",
    widen: "нажмите w: {window}",
    degraded: "не отвечают источники",
    poll_failed: "опрос не удался",
    database_read_failed: "чтение базы не удалось",
    source_list_failed: "список источников не прочитан",
    keyboard_failed: "ввод с клавиатуры прерван",
    non_http_url: "отказ открыть не-http адрес",
    browser_failed: "браузер не запустился",
    language: "язык",
    language_switched: "язык: {name}",
    help_title: " клавиши ",
    help_lines: &[
        "1 / 2 / 3        сменить окно (1 час, 24 часа, 7 дней)",
        "w, Tab           расширить окно",
        "j / k, стрелки   выбор строки",
        "g / G, Home/End  в начало / в конец",
        "[ / ]            выбрать издание, которое откроет Enter",
        "Enter            открыть статью этого издания в браузере",
        "d                открыть выбранную новость отдельно, с прокруткой",
        "s                сменить порядок сортировки",
        "l                сменить язык (English, Azərbaycan dili, Русский)",
        "/                фильтр по тексту: Backspace удаляет, Enter сохраняет, Esc очищает",
        "r                опросить сейчас",
        "?                закрыть эту справку",
        "j / k            прокрутить строки открытого окна",
        "Esc              закрыть открытое, иначе выход",
        "q                выход",
    ],
    help_scroll_hint: "j/k прокрутка · Esc закрывает",
    details_title: " новость ",
    filter_label: "фильтр",
    footer_keys: &[
        ("1-3", "окна"),
        ("j/k", "выбор"),
        ("Enter", "открыть"),
        ("/", "фильтр"),
        ("l", "язык"),
        ("s", "сортировка"),
        ("d", "подробно"),
        ("r", "обновить"),
    ],
    footer_ways_out: &[("?", "помощь"), ("q", "выход")],
    footer_filter_keys: &[("Backspace", "удаляет")],
    footer_filter_ways_out: &[("Enter", "сохранить"), ("Esc", "очистить")],
    footer_details_keys: &[("j/k", "прокрутка"), ("[/]", "издание")],
    footer_details_ways_out: &[("Esc", "назад"), ("q", "выход")],
    footer_help_keys: &[("j/k", "прокрутка")],
    footer_help_ways_out: &[("Esc", "закрыть"), ("q", "выход")],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_language_code_parses_with_or_without_a_region() {
        assert_eq!(Lang::parse("AZ"), Some(Lang::Az));
        assert_eq!(Lang::parse("ru_RU.UTF-8"), Some(Lang::Ru));
        assert_eq!(Lang::parse(" en-GB "), Some(Lang::En));
        assert_eq!(Lang::parse("de"), None);
        assert_eq!(Lang::parse(""), None);
    }

    /// Russian is the only one of the three that inflects a counted noun, and it is the one a
    /// naive `{n} слов` gets wrong.
    #[test]
    fn russian_forms_a_counted_noun_three_ways() {
        let ru = Lang::Ru.strings();
        assert_eq!(ru.outlets(1), "1 издание");
        assert_eq!(ru.outlets(2), "2 издания");
        assert_eq!(ru.outlets(5), "5 изданий");
        assert_eq!(ru.outlets(11), "11 изданий");
        assert_eq!(ru.outlets(21), "21 издание");
        assert_eq!(ru.stories(2), "2 новости");
        assert_eq!(ru.views(2170), "2170 просмотров");
    }

    #[test]
    fn english_and_azerbaijani_do_not_invent_forms_they_do_not_have() {
        assert_eq!(Lang::En.strings().outlets(1), "1 outlet");
        assert_eq!(Lang::En.strings().outlets(2), "2 outlets");
        // Azerbaijani leaves the noun alone after a numeral.
        assert_eq!(Lang::Az.strings().outlets(1), "1 sayt");
        assert_eq!(Lang::Az.strings().outlets(7), "7 sayt");
    }

    /// Which form a count takes is the language's own rule, and each one is read from a number
    /// the other two would get wrong: English counts `21 stories` where Russian counts
    /// `21 издание`, and Azerbaijani writes one form for both.
    #[test]
    fn every_language_reads_its_own_number_for_the_form() {
        let en = Lang::En.strings();
        assert_eq!(en.stories(0), "0 stories");
        assert_eq!(en.stories(1), "1 story");
        assert_eq!(en.stories(2), "2 stories");
        // The bug the Slavic rule hid: English reads the whole number, not its last digit.
        assert_eq!(en.stories(21), "21 stories");
        assert_eq!(en.stories(101), "101 stories");
        assert_eq!(en.stories(1_221), "1221 stories");

        let ru = Lang::Ru.strings();
        assert_eq!(ru.stories(21), "21 новость");
        assert_eq!(ru.stories(22), "22 новости");
        assert_eq!(ru.stories(25), "25 новостей");

        let az = Lang::Az.strings();
        assert_eq!(az.stories(1), "1 xəbər");
        assert_eq!(az.stories(21), "21 xəbər");
        assert_eq!(az.stories(1_221), "1221 xəbər");
    }

    #[test]
    fn every_language_lines_up_its_help_with_its_footer() {
        let keys = |keys: &[Keycap]| keys.iter().map(|(key, _)| *key).collect::<Vec<_>>();
        for lang in Lang::ALL {
            let t = lang.strings();
            // Every key the footer offers is explained by the help in the same language: a key
            // that works but is mentioned nowhere is a key the screen hides. The footer writes a
            // range as `1-3` and a pair as `j/k`, so a key is looked up by its first word.
            let explained = |key: &str| {
                // A one-character key is the key: `/` must not be split into nothing.
                let needle = if key.chars().count() > 1 {
                    key.split(['-', '/']).next().unwrap_or(key)
                } else {
                    key
                };
                t.help_lines
                    .iter()
                    .any(|line| line.split_whitespace().next() == Some(needle))
            };
            for (key, _) in t
                .footer_keys
                .iter()
                .chain(t.footer_ways_out)
                .chain(t.footer_details_keys)
                .chain(t.footer_details_ways_out)
            {
                assert!(
                    explained(key),
                    "{lang:?}: the footer offers {key} but the help never mentions it"
                );
            }
            // The filter's own mode is reached by the help line for `/`, which also explains it.
            assert!(
                t.help_lines.iter().any(|line| line.contains("Backspace")),
                "{lang:?}"
            );
            // The switcher is reachable from the screen without the help, and named in both
            // places. `l` is the one key whose label changes with the language it switches.
            assert!(
                keys(t.footer_keys).contains(&"l"),
                "{lang:?}: the footer must offer the language switch"
            );
        }
    }

    #[test]
    fn every_language_agrees_which_key_switches_the_language() {
        for lang in Lang::ALL {
            let t = lang.strings();
            assert_ne!(t.filter_label, "", "{lang:?}");
            assert!(
                !t.footer_filter_keys.iter().any(|(key, _)| *key == "l"),
                "in filter mode `l` is filter text, not a switch: {lang:?}"
            );
        }
    }

    /// One key has to reach all three languages and come back, or two of them need a restart.
    #[test]
    fn the_language_cycle_visits_every_language_once_and_returns() {
        let mut lang = Lang::En;
        let mut seen = vec![lang];
        for _ in 0..3 {
            lang = lang.next();
            seen.push(lang);
        }
        assert_eq!(seen, vec![Lang::En, Lang::Az, Lang::Ru, Lang::En]);

        // A name is what the user reads back after switching, so each one is spelled the way
        // its own speakers spell it.
        assert_eq!(Lang::En.name(), "English");
        assert_eq!(Lang::Az.name(), "Azərbaycan dili");
        assert_eq!(Lang::Ru.name(), "Русский");
        assert_eq!(
            Lang::Ru.strings().switched(Lang::Ru.name()),
            "язык: Русский"
        );
        assert_eq!(
            Lang::En.strings().switched(Lang::En.name()),
            "language: English"
        );
    }

    #[test]
    fn a_duration_is_written_the_way_its_language_writes_one() {
        assert_eq!(Lang::En.strings().ago(0), "just now");
        assert_eq!(Lang::En.strings().ago(720), "12m");
        assert_eq!(Lang::En.strings().ago_sentence(720), "12m ago");
        assert_eq!(Lang::Az.strings().ago_sentence(720), "12 dəq əvvəl");
        assert_eq!(Lang::Az.strings().ago(7200), "2 saat");
        assert_eq!(Lang::Ru.strings().ago_sentence(7200), "2 ч назад");
        assert_eq!(Lang::Ru.strings().ago_sentence(432_000), "5 дн назад");
    }

    /// A column has the width of its widest real value, and `только что` is not one of them.
    #[test]
    fn a_column_writes_a_minute_old_story_as_a_bound() {
        assert_eq!(Lang::En.strings().age(5), "<1m");
        assert_eq!(Lang::Az.strings().age(5), "<1 dəq");
        assert_eq!(Lang::Ru.strings().age(5), "<1 мин");
        assert_eq!(Lang::En.strings().age(720), "12m");
        assert_eq!(Lang::Az.strings().age(86_400 * 3), "3 gün");
        assert_eq!(Lang::Ru.strings().age(86_400 * 3), "3 дн");
        assert_eq!(
            Lang::En.strings().age(0),
            "<1m",
            "a story from the future is still not a phrase"
        );
    }

    #[test]
    fn a_sentence_keeps_its_own_word_order() {
        assert_eq!(
            Lang::En.strings().moved(Some(2), "24h"),
            "+2 since the previous 24h window"
        );
        assert_eq!(
            Lang::Az.strings().moved(Some(2), "24s"),
            "+2 · əvvəlki 24s aralığından bəri"
        );
        assert_eq!(
            Lang::Ru.strings().moved(None, "24ч"),
            "новое с прошлого окна 24ч"
        );
        assert_eq!(Lang::Ru.strings().picked_up(0, 14_400), "нет за 4 ч");
        assert_eq!(Lang::En.strings().picked_up(2, 1200), "+2 in 20m");
    }

    #[test]
    fn the_health_line_says_so_when_no_poll_has_reported() {
        assert_eq!(Lang::En.strings().health(Some(19), 20), "19/20 sources ok");
        assert_eq!(
            Lang::Az.strings().health(Some(19), 20),
            "19/20 mənbə işləyir"
        );
        assert_eq!(Lang::En.strings().health(None, 20), "sources unknown");
    }
}
