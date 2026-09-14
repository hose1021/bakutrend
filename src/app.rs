//! Application state. All mutable state lives here; `ui::draw` is stateless and
//! `main` is a thin composition root.

use std::collections::HashMap;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};

use crate::cluster::{Clusterer, Group};
use crate::config::Config;
use crate::error::StoreError;
use crate::i18n::Lang;
use crate::poller::PollReport;
use crate::score::{Baselines, ScoredStory, rank};
use crate::store::{ItemRow, Sample, SnapshotStory, Store, Window};
use crate::ui::View;

/// Below this many stories in the active window, the screen offers to widen the period. It
/// never widens it on its own: a list that silently becomes a different period is a list whose
/// title and contents disagree, and the filter's own count would decide when that happens.
pub const THIN_WINDOW: usize = 3;

/// The tick re-ranks at most once per this many seconds of injected time (I2).
const REFRESH_INTERVAL_SECS: i64 = 60;

/// How far back channel baselines are measured. A week gives a busy channel hundreds of rated
/// posts and a quiet one enough to clear the minimum, while staying recent enough to describe
/// how the channel behaves now.
const BASELINE_SECONDS: i64 = 7 * 86_400;

/// How many windows of history the grouping reaches back. The active window ranks what happened
/// inside it, but freshness and first arrivals are read from the whole grouping, so the context
/// has to be wider than the window or a story that started before it would look new. Two
/// windows also bound the work: the shorter windows cost the same either way, and the week
/// window stays within a fortnight of items.
const CONTEXT_WINDOWS: i64 = 2;

/// How long stored rankings are kept. They exist to answer "how did this move since the
/// previous window", which the longest window answers with a week and a half of history.
const SNAPSHOT_RETENTION_SECS: i64 = 10 * 86_400;

/// How far a stored ranking may sit from exactly one window before `now`. Snapshots are written
/// once per poll cycle, and the moment asked about is `now - window`, so the two rarely coincide.
/// A quarter of the window is wide enough for the poll cadence and narrow enough that the
/// comparison is still the previous window.
fn snapshot_tolerance(window: Window) -> i64 {
    (window.seconds() / 4).max(900)
}

/// How much of a story's words must be shared with a stored ranking's key for the two to be the
/// same story. A headline is re-ingested whenever its source rewrites it, and the group's key is
/// built from its words, so a corrected title would otherwise make a story look new. Lexical
/// overlap is the evidence the grouping itself uses, held here at a stricter level: the two are
/// known to be the same story, so only a real rewrite should separate them.
const MATCH_SIMILARITY: f64 = 0.50;

/// Where the keyboard is. One value, because the keys mean different things in each and an
/// overlay that silently keeps handling list keys is a screen the user cannot leave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The ranked list with the card beside it.
    List,
    /// The text filter, where every printable key is filter text.
    Filter,
    /// The help overlay, scrollable.
    Help,
    /// The selected story alone, scrolled and with its sources selectable.
    Details,
}

/// The order the list is shown in.
///
/// The index is the program's own ranking. The others read the same scored stories through
/// different lenses, so switching one changes the order and never a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// The prominence index, best first.
    Index,
    /// Recent relative growth: how far above their channels' measured pace the story's posts run.
    Growth,
    /// Breadth of coverage: how much independent reporting the story carries.
    Coverage,
    /// Telegram views the story's posts had accumulated when they were last sampled.
    Views,
    /// Newest first, by the latest report.
    Chronology,
}

impl Sort {
    pub const ALL: [Sort; 5] = [
        Sort::Index,
        Sort::Growth,
        Sort::Coverage,
        Sort::Views,
        Sort::Chronology,
    ];

    pub fn next(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// The value the list is ordered by. Every sort is descending: the biggest number is the
    /// most interesting one in each of them.
    fn key(self, story: &ScoredStory) -> f64 {
        match self {
            Sort::Index => story.score,
            Sort::Growth => story.engagement,
            Sort::Coverage => story.coverage,
            Sort::Views => story.view_count as f64,
            Sort::Chronology => story.newest as f64,
        }
    }
}

#[derive(Debug)]
pub enum AppEvent {
    Input(Event),
    PollDone(PollReport),
    PollFailed(String),
    /// The input thread died; the TUI must say so instead of going deaf (I12).
    InputFailed(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    OpenUrl(String),
    ForcePoll,
}

pub struct App {
    pub store: Store,
    pub config: Config,
    /// The injected clock. Key handling refreshes at this time, never at the wall clock,
    /// so a test that drives keys controls the time those refreshes use.
    pub now: i64,
    pub window: Window,
    /// The resolved interface language. `Config::load` refuses a language it does not know, so
    /// the fallback here only covers a `Config` built in code, such as a test's.
    pub lang: Lang,
    pub stories: Vec<ScoredStory>,
    /// The full stored text of the selected story's items, keyed by URL. Loaded when the details
    /// view opens, because a ranked story carries only a lede: the whole post is in the store,
    /// and reading it costs a query that the list never needs.
    pub bodies: HashMap<String, String>,
    /// Stories the active window holds before the text filter: what the filter is narrowing.
    pub window_total: usize,
    pub deltas: Option<HashMap<String, i64>>,
    /// When the stored ranking the movement column compares against was computed. `None`
    /// together with `deltas`.
    pub comparison_at: Option<i64>,
    pub selected: usize,
    /// Which of the selected story's outlets `Enter` opens. Moved with `[` and `]`, so the
    /// program never opens a publication the user did not choose.
    pub outlet: usize,
    pub filter: String,
    pub mode: Mode,
    /// The order the list is shown in. Never affects a score.
    pub sort: Sort,
    /// First line of the details view, and of the help overlay.
    pub scroll: u16,
    pub help_scroll: u16,
    pub sources_ok: Option<usize>,
    pub sources_total: usize,
    pub last_poll: Option<i64>,
    /// Source names that failed and have not succeeded again (I10).
    pub degraded: Vec<String>,
    /// The injected clock of the last ranking, for the tick throttle (I2).
    pub last_refresh: Option<i64>,
    pub status: String,
}

impl App {
    pub fn new(store: Store, config: Config, now: i64) -> Self {
        // The Google seed is not a polled source, so it does not count toward health.
        let lang = Lang::parse(&config.language).unwrap_or(Lang::En);
        let (sources_total, enumeration_status) = match store.sources(true) {
            Ok(rows) => (
                rows.iter()
                    .filter(|row| row.kind != crate::source::SourceKind::Google)
                    .count(),
                String::new(),
            ),
            Err(error) => (0, format!("{}: {error}", lang.strings().source_list_failed)),
        };
        let mut app = Self {
            store,
            lang,
            config,
            now,
            window: Window::Day,
            stories: Vec::new(),
            bodies: HashMap::new(),
            window_total: 0,
            deltas: None,
            comparison_at: None,
            selected: 0,
            outlet: 0,
            filter: String::new(),
            mode: Mode::List,
            sort: Sort::Index,
            scroll: 0,
            help_scroll: 0,
            sources_ok: None,
            sources_total,
            last_poll: None,
            degraded: Vec::new(),
            last_refresh: None,
            status: enumeration_status,
        };
        app.refresh(now);
        app.last_refresh = Some(now);
        app
    }

    /// Regroup and rerank. Called when data or the window changes, not on every tick.
    ///
    /// The window decides what is ranked, and the filter only narrows what is shown. Neither
    /// changes the other: a filter that silently switched the period would leave the header
    /// naming a range the list is not showing.
    pub fn refresh(&mut self, now: i64) {
        self.now = now;
        let Some(stories) = rank_window(&self.store, &self.config, self.window, now) else {
            self.status = self.lang.strings().database_read_failed.to_string();
            return;
        };
        self.window_total = stories.len();
        // The movement column is read from the ranking as computed, before the filter and before
        // any reordering: it claims how a story moved in the index, whatever order is on screen.
        let comparison = self.compute_deltas(&stories, now);
        self.comparison_at = comparison.as_ref().map(|(_, at)| *at);
        self.deltas = comparison.map(|(deltas, _)| deltas);
        self.stories = sort_stories(self.apply_filters(stories), self.sort);
        self.clamp_selection();
        // Only the details view reads whole bodies, and a refresh happens on every keystroke of
        // the filter. Loading them for a list screen would be one query per character typed.
        if self.mode == Mode::Details {
            self.load_bodies();
        }
    }

    /// Point the model at what a caller asked for: the window, the order, the text filter and
    /// the story the card explains. One entry point, so a second front end reaches the ranking
    /// the TUI draws instead of building its own.
    ///
    /// The four move together because they are one view: a refresh ranks the window, sorts the
    /// result and narrows it once, and setting them apart would rank the same window three times
    /// and leave the screen between the three states.
    pub fn set_view(&mut self, window: Window, sort: Sort, filter: String, selected: usize) {
        self.window = window;
        self.sort = sort;
        self.filter = filter;
        self.selected = selected;
        self.outlet = 0;
        self.scroll = 0;
        self.refresh(self.now);
    }

    /// Read the selected story's full texts out of the store, one per outlet URL.
    ///
    /// The story itself carries a lede of three hundred characters. This is the same text as the
    /// sources published it, and the details view is the only place with the room to show it.
    /// A read failure leaves the cache empty rather than replacing the screen with an error: the
    /// view falls back to the lede, which is the text it would have shown anyway.
    pub fn load_bodies(&mut self) {
        let urls: Vec<String> = self
            .stories
            .get(self.selected)
            .map(|story| {
                story
                    .outlets
                    .iter()
                    .map(|outlet| outlet.url.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.bodies = self.store.bodies_by_url(&urls).unwrap_or_default();
    }

    /// Keep the selection, the chosen outlet and the scroll inside what is on screen. A refresh
    /// can shrink any of them, and a cursor past the end is a screen with nothing selected.
    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.stories.len().saturating_sub(1));
        let outlets = self
            .stories
            .get(self.selected)
            .map_or(0, |story| story.outlets.len());
        self.outlet = self.outlet.min(outlets.saturating_sub(1));
    }

    /// Retain the ranked stories whose title matches the filter. The ranking itself was computed
    /// over the whole window on purpose: a filter narrows what is shown, it must not move the
    /// scores, and it must not change the period either.
    fn apply_filters(&self, stories: Vec<ScoredStory>) -> Vec<ScoredStory> {
        let needle = crate::text::fold(&self.filter);
        if needle.is_empty() {
            return stories;
        }
        stories
            .into_iter()
            .filter(|story| crate::text::fold(&story.title).contains(&needle))
            .collect()
    }

    /// Express the change in position since the stored ranking of one window ago (I4).
    ///
    /// `None` means there is nothing comparable, which hides the column. The comparison is
    /// against a snapshot that was actually computed and written down — never against a ranking
    /// recomputed from today's rows, which would let a headline corrected this morning, a
    /// citation detected today or a view sample taken since change what the past "was". Until a
    /// comparable snapshot exists, the program reports no movement at all.
    fn compute_deltas(
        &self,
        current: &[ScoredStory],
        now: i64,
    ) -> Option<(HashMap<String, i64>, i64)> {
        if current.is_empty() {
            return None;
        }
        let at = now - self.window.seconds();
        let snapshot = self
            .store
            .snapshot_near(
                self.window,
                crate::score::ALGORITHM_VERSION,
                at,
                snapshot_tolerance(self.window),
            )
            .ok()??;
        if snapshot.stories.is_empty() {
            return None;
        }
        let deltas = current
            .iter()
            .enumerate()
            .filter_map(|(index, story)| {
                let previous = match_snapshot(&story.key, &snapshot.stories)?;
                Some((story.key.clone(), previous.rank - index as i64))
            })
            .collect();
        Some((deltas, snapshot.computed_at))
    }

    pub fn record_poll(&mut self, report: &PollReport, now: i64) {
        self.now = now;
        self.last_poll = Some(now);
        self.sources_ok = Some(report.ok);
        for (name, _) in &report.failed {
            if !self.degraded.contains(name) {
                self.degraded.push(name.clone());
            }
        }
        for name in &report.succeeded {
            self.degraded.retain(|degraded| degraded != name);
        }
        self.status = self.compose_status("");
        self.refresh(now);
    }

    /// The status line names every currently degraded source, whatever else went wrong;
    /// a source in backoff must not vanish from every surface (I10).
    fn compose_status(&self, message: &str) -> String {
        let degraded = if self.degraded.is_empty() {
            String::new()
        } else {
            self.lang
                .strings()
                .degraded_status(self.degraded.len(), &self.degraded.join(", "))
        };
        match (message.is_empty(), degraded.is_empty()) {
            (true, true) => String::new(),
            (true, false) => degraded,
            (false, true) => message.to_string(),
            (false, false) => format!("{message} · {degraded}"),
        }
    }

    /// Advance the injected clock and re-rank when at least a minute of injected time
    /// has passed since the last ranking (I2): fresh enough to track the window, rare
    /// enough not to re-query several times a second.
    pub fn tick(&mut self, now: i64) {
        self.now = now;
        if self
            .last_refresh
            .is_none_or(|last| now - last >= REFRESH_INTERVAL_SECS)
        {
            self.refresh(now);
            self.last_refresh = Some(now);
        }
    }

    /// Advance the injected clock. `refresh` and `record_poll` also set it from their
    /// own `now` argument.
    pub fn set_now(&mut self, now: i64) {
        self.now = now;
    }

    pub fn view(&self) -> View<'_> {
        View {
            lang: self.lang,
            window: self.window,
            stories: &self.stories,
            bodies: &self.bodies,
            window_total: self.window_total,
            selected: self.selected,
            outlet: self.outlet,
            deltas: self.deltas.as_ref(),
            comparison_at: self.comparison_at,
            filter: &self.filter,
            mode: self.mode,
            sort: self.sort,
            scroll: self.scroll,
            help_scroll: self.help_scroll,
            sources_ok: self.sources_ok,
            sources_total: self.sources_total,
            last_poll: self.last_poll,
            degraded: &self.degraded,
            // Every displayed time comes from the one injected clock (I2).
            now: self.now,
            status: &self.status,
        }
    }

    pub fn handle(&mut self, event: AppEvent) -> Action {
        match event {
            AppEvent::Input(Event::Key(key)) => self.handle_key(key),
            AppEvent::Input(Event::Resize(..)) => Action::None,
            AppEvent::Input(_) => Action::None,
            AppEvent::PollDone(report) => {
                self.record_poll(&report, self.now);
                Action::None
            }
            AppEvent::PollFailed(message) => {
                // The error's own words are the system's, not ours: only the prefix is
                // translated, so a Russian screen does not misreport what failed.
                let message = format!("{}: {message}", self.lang.strings().poll_failed);
                self.status = self.compose_status(&message);
                Action::None
            }
            AppEvent::InputFailed(message) => {
                self.status = self.compose_status(&message);
                Action::None
            }
        }
    }

    /// Every key, one dispatch per mode. A key means one thing at a time, and every overlay has
    /// one visible way out: `Esc` closes what is open, and quits only when nothing is.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        match self.mode {
            Mode::Filter => {
                self.handle_filter_key(key);
                Action::None
            }
            Mode::Help => self.handle_help_key(key),
            Mode::Details => self.handle_details_key(key),
            Mode::List => self.handle_list_key(key),
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::List;
                self.refresh(self.now);
            }
            KeyCode::Enter => self.mode = Mode::List,
            KeyCode::Backspace => {
                self.filter.pop();
                self.refresh(self.now);
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.refresh(self.now);
            }
            _ => {}
        }
    }

    /// The help overlay scrolls rather than clips: on a twelve-row terminal the keys below the
    /// fold are the ones a user needs, and dropping them silently is the bug this replaces.
    fn handle_help_key(&mut self, key: KeyEvent) -> Action {
        let lines = self.lang.strings().help_lines.len() as u16 + 2;
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc => {
                self.mode = Mode::List;
                self.help_scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.help_scroll = self.help_scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
            }
            KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(5),
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(5),
            KeyCode::Char('g') | KeyCode::Home => self.help_scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.help_scroll = lines.saturating_sub(1),
            KeyCode::Char('?') | KeyCode::Enter => {
                self.mode = Mode::List;
                self.help_scroll = 0;
            }
            _ => {}
        }
        Action::None
    }

    /// The selected story alone, scrolled. This is how every source and every signal stays
    /// reachable when the terminal is too short to show them beside the list.
    fn handle_details_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('d') => {
                self.mode = Mode::List;
                self.scroll = 0;
            }
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('j') | KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => self.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.scroll = u16::MAX,
            KeyCode::Char('[') => self.move_outlet(-1),
            KeyCode::Char(']') => self.move_outlet(1),
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Enter => return self.open_selected_outlet(),
            _ => {}
        }
        Action::None
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc => return Action::Quit,
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < self.stories.len() {
                    self.selected += 1;
                    self.outlet = 0;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.outlet = 0;
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected = 0;
                self.outlet = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.selected = self.stories.len().saturating_sub(1);
                self.outlet = 0;
            }
            KeyCode::Char('1') => self.switch_window(Window::Hour),
            KeyCode::Char('2') => self.switch_window(Window::Day),
            KeyCode::Char('3') => self.switch_window(Window::Week),
            // An explicit widening. The program never widens the period on its own, so the
            // number of stories a filter happens to match cannot change what is being ranked.
            KeyCode::Char('w') | KeyCode::Tab => self.switch_window(self.window.wider()),
            KeyCode::Char('l') => self.switch_language(),
            KeyCode::Char('s') => self.switch_sort(),
            // The view reads the whole body of each source, so the query happens here rather than
            // in the draw loop, which runs on every tick.
            KeyCode::Char('d') => {
                self.mode = Mode::Details;
                self.load_bodies();
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('[') => self.move_outlet(-1),
            KeyCode::Char(']') => self.move_outlet(1),
            KeyCode::Char('r') => return Action::ForcePoll,
            KeyCode::Enter => return self.open_selected_outlet(),
            _ => {}
        }
        Action::None
    }

    /// Open the publication the user picked. `Enter` never falls back to the first outlet in the
    /// list: which one it opens is shown by the marker, and moving that marker is a keypress.
    fn open_selected_outlet(&mut self) -> Action {
        self.stories
            .get(self.selected)
            .and_then(|story| story.outlets.get(self.outlet))
            .map(|outlet| Action::OpenUrl(outlet.url.clone()))
            .unwrap_or(Action::None)
    }

    /// Move the publication marker within the selected story, stopping at both ends. A key that
    /// silently wraps would open a different outlet than the marker showed.
    fn move_outlet(&mut self, step: i64) {
        let count = self
            .stories
            .get(self.selected)
            .map_or(0, |story| story.outlets.len());
        if count == 0 {
            return;
        }
        let next = self.outlet as i64 + step;
        self.outlet = next.clamp(0, count as i64 - 1) as usize;
    }

    /// Cycle the list order. Scores do not change, so the card stays comparable.
    fn switch_sort(&mut self) {
        self.sort = self.sort.next();
        self.stories = sort_stories(std::mem::take(&mut self.stories), self.sort);
        self.selected = 0;
        self.outlet = 0;
        self.clamp_selection();
        let name = self.lang.strings().sort(self.sort).to_string();
        self.status = self.lang.strings().sorted(&name);
    }

    /// Cycle the interface language. Only the chrome changes: headlines keep the language their
    /// outlet wrote them in, which is why the switch says which language is now active instead
    /// of implying the stories were translated too.
    fn switch_language(&mut self) {
        self.lang = self.lang.next();
        self.status = self.lang.strings().switched(self.lang.name());
    }

    fn switch_window(&mut self, window: Window) {
        if self.window != window {
            self.window = window;
            self.selected = 0;
            self.outlet = 0;
            self.scroll = 0;
            self.refresh(self.now);
        }
    }
}

/// The measurements a ranking at `t` is allowed to see: never one taken after `t`.
///
/// `window_data` selects samples by their item, not by their own timestamp, so an item inside a
/// range can carry samples from outside it. The later ones belong to a later ranking; letting
/// them into an earlier one rewrites a result the user has already seen.
fn samples_at(samples: &[Sample], t: i64) -> Vec<Sample> {
    samples
        .iter()
        .copied()
        .filter(|sample| sample.ts <= t)
        .collect()
}

/// The groups whose items fall inside `[from, to]`, each trimmed to those items. The
/// key and tokens stay the shared grouping's, so identity is stable across windows
/// and across the current/previous rankings (I1).
fn window_slice(groups: &[Group], from: i64, to: i64) -> Vec<Group> {
    groups
        .iter()
        .filter_map(|group| {
            let items: Vec<ItemRow> = group
                .items
                .iter()
                .filter(|item| item.published_at >= from && item.published_at <= to)
                .cloned()
                .collect();
            if items.is_empty() {
                return None;
            }
            let mut sliced = group.clone();
            sliced.items = items;
            sliced.item_ids = sliced.items.iter().map(|item| item.item_id).collect();
            sliced.newest = sliced.items.iter().map(|item| item.published_at).max()?;
            sliced.oldest = sliced
                .items
                .iter()
                .map(|item| item.published_at)
                .min()
                .unwrap_or(sliced.oldest);
            Some(sliced)
        })
        .collect()
}

/// The prominence ranking of one window as of `now`, computed from stored data only.
///
/// Shared with the poller, which records the result as a snapshot: the stored ranking and the one
/// on screen must come from one implementation, or the movement column would compare two
/// different rankings and report the difference as news. `None` when the read fails.
pub fn rank_window(
    store: &Store,
    config: &Config,
    window: Window,
    now: i64,
) -> Option<Vec<ScoredStory>> {
    let span = window.seconds();
    let (context, samples) = store
        .window_data(now - CONTEXT_WINDOWS * span, now, window.allows_backfill())
        .ok()?;
    // A sample is selected by the item it belongs to, not by its own timestamp, so the rows this
    // returns can include measurements taken after `now`. A ranking at `now` reads none of them.
    let samples = samples_at(&samples, now);
    let embeddings = load_embeddings(store, now - CONTEXT_WINDOWS * span, now);
    let clusterer = Clusterer::new(config.cluster_threshold)
        .with_semantic(config.semantic_threshold, embeddings);
    // One grouping over the whole context: a story's key must not depend on which window is
    // active (I1), and freshness and first arrivals are read from the group's full history.
    let groups = clusterer.group_items(&context);
    let baselines = baselines_at(store, now).unwrap_or_else(Baselines::empty);
    let current = window_slice(&groups, now - span, now);
    Some(rank(
        &current,
        &samples,
        &baselines,
        window,
        &config.weights,
        now,
    ))
}

/// Compute and store the ranking of every window as it stands at `now`.
///
/// Called by the poller after each cycle, on the connection that is allowed to write. This is
/// the only place a ranking is written down, so the movement column compares against a ranking
/// that was really computed at a real time rather than recomputed later from changed rows.
/// Nothing happens while the program is not running, and that is the honest answer: a period
/// nobody observed has no ranking to compare against.
pub fn record_rankings(store: &mut Store, config: &Config, now: i64) -> Result<usize, StoreError> {
    let mut written = 0;
    for window in Window::all() {
        let Some(stories) = rank_window(store, config, window, now) else {
            continue;
        };
        let rows: Vec<(String, f64)> = stories
            .iter()
            .map(|story| (story.key.clone(), story.score))
            .collect();
        written += store.save_snapshot(now, crate::score::ALGORITHM_VERSION, window, &rows)?;
    }
    store.prune_snapshots(now - SNAPSHOT_RETENTION_SECS)?;
    Ok(written)
}

/// Channel baselines as they stand at `t`, from a week of history that ends then.
///
/// A channel's typical pace is a property of the channel, not of the hour being ranked: a
/// one-hour sample would fall back to the corpus median for nearly every channel, which is
/// exactly the normalization this is supposed to avoid.
///
/// `None` when the read fails: the caller then ranks against the empty baseline and still shows
/// a list, which is better than showing nothing.
fn baselines_at(store: &Store, t: i64) -> Option<Baselines> {
    let (items, samples) = store.window_data(t - BASELINE_SECONDS, t, true).ok()?;
    Some(Baselines::from_items(&items, &samples_at(&samples, t), t))
}

/// Cached vectors for a range, or an empty map when there are none.
///
/// The cache names its own model — the most recently written one — so nothing needs configuring
/// for this to start working, and two models are never compared by accident. No vectors is the
/// ordinary state, and the clusterer then decides lexically.
fn load_embeddings(store: &Store, from: i64, to: i64) -> HashMap<i64, Vec<f32>> {
    let Ok(Some(model)) = store.current_embedding_model() else {
        return HashMap::new();
    };
    store
        .embeddings_for_range(from, to, &model)
        .unwrap_or_default()
}

/// Order a scored list for display. The index order is left alone: `rank` already returns it best
/// first, and its tie-break on recency is part of the ranking. Every other order is descending
/// with the same deterministic tie-breaks, so the same data always draws the same screen.
fn sort_stories(mut stories: Vec<ScoredStory>, sort: Sort) -> Vec<ScoredStory> {
    if sort != Sort::Index {
        stories.sort_by(|a, b| {
            sort.key(b)
                .total_cmp(&sort.key(a))
                .then(b.newest.cmp(&a.newest))
                .then(a.key.cmp(&b.key))
        });
    }
    stories
}

/// The story in a stored ranking that a current key refers to.
///
/// The key is the group's token set, and a token set is not immutable: re-ingesting a source
/// rewrites its headlines, and an editor can fix a title between two snapshots. A key that no
/// longer matches exactly is therefore looked up by word overlap, the same evidence the grouping
/// itself uses. Nothing is guessed: a key with no match is a new story, and the screen says so.
fn match_snapshot<'a>(key: &str, stories: &'a [SnapshotStory]) -> Option<&'a SnapshotStory> {
    if let Some(story) = stories.iter().find(|story| story.key == key) {
        return Some(story);
    }
    let current = key_tokens(key)?;
    let mut best: Option<(&SnapshotStory, f64)> = None;
    for story in stories {
        let Some(stored) = key_tokens(&story.key) else {
            continue;
        };
        let score = crate::cluster::similarity(&current, &stored);
        if score >= MATCH_SIMILARITY && best.is_none_or(|(_, top)| score > top) {
            best = Some((story, score));
        }
    }
    best.map(|(story, _)| story)
}

/// The token set behind a story key, or `None` for a key that is not one.
///
/// `signature` joins tokens with `-` and no token contains a `-`, so the join is reversible. An
/// untokenized story is keyed by its row id instead and has no token set to compare.
fn key_tokens(key: &str) -> Option<Vec<String>> {
    if key.starts_with("untokenized:") {
        return None;
    }
    let tokens: Vec<String> = key
        .split('-')
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect();
    (!tokens.is_empty()).then_some(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{ParsedItem, SourceKind, SourceSpec};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    const NOW: i64 = 1_700_000_000;
    const DAY: i64 = 86_400;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// An enabled RSS source in an otherwise empty store.
    fn seed_source(store: &mut Store) -> i64 {
        store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Rss,
                    outlet: "APA".into(),
                    name: "APA RSS".into(),
                    locator: "https://apa.az/rss".into(),
                },
                true,
            )
            .unwrap()
    }

    /// One parsed item for the synthetic fixtures.
    fn parsed(external_id: &str, title: &str, published_at: i64) -> ParsedItem {
        ParsedItem {
            external_id: external_id.to_string(),
            url: format!("https://apa.az/incident/{external_id}"),
            title: title.to_string(),
            description: None,
            section: None,
            published_at,
            views: None,
            cited: false,
            cited_outlet: None,
            publisher: None,
        }
    }

    fn story_keys(app: &App) -> Vec<String> {
        app.stories.iter().map(|story| story.key.clone()).collect()
    }

    fn app_with_stories() -> App {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Rss,
                    outlet: "APA".into(),
                    name: "APA RSS".into(),
                    locator: "https://apa.az/rss".into(),
                },
                true,
            )
            .unwrap();
        // Deliberately distinct headlines: a shared vocabulary would merge them into one story.
        let headlines = [
            "Bakıda metro stansiyasında təmir işləri başladı",
            "Gəncədə toy karvanı qəza etdi, yaralılar var",
            "Sumqayıtda zavod yanğını söndürüldü",
            "Qarabağda diplomatik görüş keçirildi",
            "Naxçıvanda yeni magistral yol açıldı",
        ];
        let items: Vec<ParsedItem> = headlines
            .iter()
            .enumerate()
            .map(|(index, headline)| ParsedItem {
                external_id: format!("id{index}"),
                url: format!("https://apa.az/incident/x-{index}"),
                title: headline.to_string(),
                description: None,
                section: Some("incident".into()),
                published_at: NOW - index as i64 * 600,
                views: None,
                cited: false,
                cited_outlet: None,
                publisher: None,
            })
            .collect();
        store.upsert_items(source_id, &items, NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        // The language this fixture's screen speaks, stated here rather than inherited from
        // whatever the built-in default happens to be: the tests that assert a sentence are
        // about the sentence, and a test about another language sets its own.
        app.lang = Lang::En;
        app.refresh(NOW);
        app
    }

    #[test]
    fn refresh_produces_one_ranked_story_per_distinct_headline() {
        let app = app_with_stories();
        assert_eq!(app.stories.len(), 5);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn movement_is_clamped_at_both_ends() {
        let mut app = app_with_stories();
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected, 0, "cannot move above the first row");
        for _ in 0..10 {
            app.handle_key(key(KeyCode::Char('j')));
        }
        assert_eq!(
            app.selected,
            app.stories.len() - 1,
            "cannot move past the last row"
        );
    }

    #[test]
    fn number_keys_switch_window_and_reset_selection() {
        let mut app = app_with_stories();
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.window, Window::Day);
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.window, Window::Hour);
        assert_eq!(app.selected, 0);
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.window, Window::Week);
    }

    /// `l` must be enough on its own: one key press, a screen that speaks the new language, and
    /// a line that says which one is active.
    #[test]
    fn l_switches_the_language_and_announces_it() {
        let mut app = app_with_stories();
        assert_eq!(app.lang, Lang::En);
        let configured = app.config.language.clone();

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.lang, Lang::Az);
        assert_eq!(app.view().lang, Lang::Az, "the screen follows the switch");
        assert_eq!(app.status, "dil: Azərbaycan dili");
        assert_eq!(
            app.config.language, configured,
            "the switch is for this session; a restart returns to the configured language"
        );

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.lang, Lang::Ru);
        assert_eq!(app.status, "язык: Русский");
        assert_eq!(
            app.stories.len(),
            5,
            "the ranking survives a re-drawn screen"
        );

        app.handle_key(key(KeyCode::Char('l')));
        assert_eq!(app.lang, Lang::En, "the cycle comes back around");
    }

    /// In filter mode every printable key is filter text, so `l` has to type an `l` there
    /// rather than switch the language out from under the query.
    #[test]
    fn l_types_into_the_filter_instead_of_switching() {
        let mut app = app_with_stories();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('l')));

        assert_eq!(app.filter, "l");
        assert_eq!(app.lang, Lang::En);
    }

    #[test]
    fn q_quits_and_enter_opens_the_selected_story() {
        let mut app = app_with_stories();
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Action::Quit);

        match app.handle_key(key(KeyCode::Enter)) {
            Action::OpenUrl(url) => assert!(url.starts_with("https://apa.az/")),
            other => panic!("expected OpenUrl, got {other:?}"),
        }
    }

    #[test]
    fn r_requests_a_poll() {
        let mut app = app_with_stories();
        assert_eq!(app.handle_key(key(KeyCode::Char('r'))), Action::ForcePoll);
    }

    #[test]
    fn question_mark_opens_help_and_escape_closes_it_before_quitting() {
        let mut app = app_with_stories();
        assert_eq!(app.mode, Mode::List);
        app.handle_key(key(KeyCode::Char('?')));
        assert_eq!(app.mode, Mode::Help);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            Action::None,
            "Esc closes help first"
        );
        assert_eq!(app.mode, Mode::List);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            Action::Quit,
            "then Esc quits"
        );
    }

    /// A thin window is reported as thin and offered a wider one. The period itself does not
    /// move until the user presses the key: a list that silently becomes a different period is a
    /// list whose tab and contents disagree.
    #[test]
    fn a_thin_window_is_not_replaced_by_a_wider_one_until_the_user_asks() {
        let mut app = app_with_stories();
        app.window = Window::Hour;
        app.refresh(NOW);
        assert_eq!(
            app.window,
            Window::Hour,
            "the hour window is still the hour"
        );

        // Push the items outside the hour window: nothing is left to show, and the screen says
        // so instead of substituting the week.
        app.refresh(NOW + 7200);
        assert_eq!(
            app.window,
            Window::Hour,
            "the period did not change by itself"
        );
        assert!(app.stories.is_empty(), "the hour really is empty");
        assert_eq!(app.window_total, 0);

        // The widening is one keypress, and it is the user's.
        app.handle_key(key(KeyCode::Char('w')));
        assert_eq!(app.window, Window::Day);
        assert!(
            !app.stories.is_empty(),
            "the day window still holds the stories the hour had gone past"
        );
        app.handle_key(key(KeyCode::Char('w')));
        assert_eq!(app.window, Window::Week);
        assert!(app.window_total > 0, "the week still holds the stories");
    }

    #[test]
    fn refresh_merges_two_reports_of_one_event_that_share_a_name() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let items = vec![
            parsed(
                "same-a",
                "İlham Əliyev metro stansiyasında təmir işləri başladı",
                NOW,
            ),
            parsed(
                "same-b",
                "İlham Əliyev metro stansiyasında təmir işləri davam edir",
                NOW - 60,
            ),
        ];
        store.upsert_items(source_id, &items, NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        app.refresh(NOW);
        assert_eq!(
            app.stories.len(),
            1,
            "one event is one story, not one story per item"
        );
    }

    /// Spec §12: two headlines can share every word but the one that matters and still be two
    /// events. A shared city is not evidence that two sentences describe the same thing.
    #[test]
    fn refresh_keeps_two_causes_in_one_city_apart() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let items = vec![
            parsed(
                "weather-a",
                "Bakıda güclü yağış səbəbindən yollar bağlandı",
                NOW,
            ),
            parsed(
                "weather-b",
                "Bakıda güclü külək səbəbindən yollar bağlandı",
                NOW - 60,
            ),
        ];
        store.upsert_items(source_id, &items, NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        app.refresh(NOW);
        assert_eq!(
            app.stories.len(),
            2,
            "the cause is the news, so a different cause is a different event"
        );
    }

    #[test]
    fn key_triggered_refresh_uses_the_injected_clock() {
        let mut app = app_with_stories();
        app.set_now(NOW);
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.window, Window::Hour);

        let mut reference = app_with_stories();
        reference.window = Window::Hour;
        reference.refresh(NOW);
        assert!(
            !app.stories.is_empty(),
            "the injected clock is inside the fixture window"
        );
        assert_eq!(
            story_keys(&app),
            story_keys(&reference),
            "the key path must refresh at the injected clock, not the wall clock"
        );
    }

    /// A ranking is compared against a stored one, and only against a stored one. With no
    /// snapshot there is nothing to compare and the movement column stays hidden.
    #[test]
    fn movement_needs_a_stored_ranking_and_shows_nothing_without_one() {
        let mut app = app_with_stories();
        app.refresh(NOW);
        assert!(
            app.deltas.is_none(),
            "nothing has been stored for a window ago, so nothing moved"
        );
        assert!(app.comparison_at.is_none());
    }

    /// The stored ranking is what the movement column reads, and it reads a whole window back,
    /// not a minute: a snapshot taken this second is not the previous window's ranking.
    #[test]
    fn a_stored_ranking_a_whole_window_back_is_what_movement_compares_against() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        // Yesterday's reports, ingested yesterday, so they are rows that moment could see.
        let mut yesterday = Vec::new();
        for (index, title) in [
            "Lənkəranda çay fabriki yenidən açıldı",
            "Şəkidə ipək emalatxanası genişləndirildi",
            "Qubada meşə yanğınına nəzarət edilir",
        ]
        .iter()
        .enumerate()
        {
            yesterday.push(parsed(
                &format!("old{index}"),
                title,
                NOW - DAY - 600 * (index as i64 + 1),
            ));
        }
        store
            .upsert_items(source_id, &yesterday, NOW - DAY)
            .unwrap();

        // The ranking as it stood then, computed and written down.
        let config = Config::default();
        record_rankings(&mut store, &config, NOW - DAY).unwrap();

        // Today's reports of the same three stories, on top of the same grouping.
        for (index, title) in [
            "Lənkəranda çay fabriki yenidən açıldı",
            "Şəkidə ipək emalatxanası genişləndirildi",
            "Qubada meşə yanğınına nəzarət edilir",
        ]
        .iter()
        .enumerate()
        {
            store
                .upsert_items(
                    source_id,
                    &[parsed(
                        &format!("new{index}"),
                        title,
                        NOW - 600 * (index as i64 + 1),
                    )],
                    NOW,
                )
                .unwrap();
        }

        let mut app = App::new(store, config, NOW);
        app.refresh(NOW);
        let deltas = app
            .deltas
            .as_ref()
            .expect("a stored ranking one window back is comparable");
        assert_eq!(app.comparison_at, Some(NOW - DAY));
        assert_eq!(
            deltas.len(),
            app.window_total,
            "every story carried by both rankings reports its movement"
        );

        // A snapshot from this second is not one window back, and is not used.
        let mut fresh = app_with_stories();
        record_rankings(&mut fresh.store, &fresh.config, NOW).unwrap();
        fresh.refresh(NOW);
        let snapshot = fresh
            .store
            .snapshot_near(Window::Day, crate::score::ALGORITHM_VERSION, NOW, 0)
            .unwrap()
            .expect("the snapshot was written");
        assert_eq!(snapshot.computed_at, NOW);
        assert!(
            fresh
                .store
                .snapshot_near(
                    Window::Day,
                    crate::score::ALGORITHM_VERSION,
                    NOW - fresh.window.seconds(),
                    snapshot_tolerance(Window::Day),
                )
                .unwrap()
                .is_none(),
            "a snapshot taken now is a day away from the moment being asked about"
        );
        assert!(
            fresh.deltas.is_none(),
            "a ranking of today is not a ranking of yesterday"
        );
    }

    #[test]
    fn slash_filter_mode_narrows_the_list_and_escape_clears_it() {
        let mut app = app_with_stories();
        app.set_now(NOW);
        app.handle_key(key(KeyCode::Char('/')));
        assert_eq!(app.mode, Mode::Filter, "slash enters filter mode");

        app.handle_key(key(KeyCode::Char('m')));
        assert_eq!(app.stories.len(), 4, "typing narrows the list");

        app.handle_key(key(KeyCode::Char('e')));
        assert_eq!(app.filter, "me");
        assert_eq!(
            app.stories.len(),
            1,
            "a second character narrows it further"
        );

        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.filter, "m");
        assert_eq!(app.stories.len(), 4, "backspace widens it again");

        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::List, "Esc leaves filter mode");
        assert!(app.filter.is_empty(), "Esc clears the filter");
        assert_eq!(app.stories.len(), 5, "the cleared filter shows every story");
    }

    fn render_app(app: &App) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| crate::ui::draw(frame, &app.view()))
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Spec §13: a cold start has not polled anything, so the header must not claim
    /// reachability it does not know.
    #[test]
    fn a_cold_start_reports_source_health_as_unknown_until_a_poll_reports() {
        let app = app_with_stories();
        assert_eq!(app.sources_ok, None);
        let screen = render_app(&app);
        assert!(!screen.contains("sources ok"), "{screen}");

        let report = PollReport {
            ok: 7,
            ..Default::default()
        };
        let mut app = app_with_stories();
        app.record_poll(&report, NOW);
        assert_eq!(app.sources_ok, Some(7));
        let screen = render_app(&app);
        assert!(screen.contains("7/1 sources ok"), "{screen}");
    }

    /// I1: one grouping over the full context range, so a story's key cannot change
    /// with the active window, or between the ranking and the stored ranking it is compared with.
    #[test]
    fn a_story_spanning_both_windows_keeps_one_key_across_rankings() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let metro = "Bakıda metro stansiyasında təmir işləri";
        let older = vec![
            parsed("old", metro, NOW - DAY - 3600),
            parsed(
                "p1",
                "Lənkəranda çay fabriki yenidən açıldı",
                NOW - DAY - 600,
            ),
            parsed(
                "p2",
                "Şəkidə ipək emalatxanası genişləndirildi",
                NOW - DAY - 1200,
            ),
            parsed(
                "p3",
                "Qubada meşə yanğınına nəzarət edilir",
                NOW - DAY - 1800,
            ),
        ];
        store.upsert_items(source_id, &older, NOW - DAY).unwrap();

        let config = Config::default();
        record_rankings(&mut store, &config, NOW - DAY).unwrap();

        let current = vec![
            parsed(
                "new",
                "Bakıda metro stansiyasında təmir işləri davam edir",
                NOW - 3600,
            ),
            // Enough current-window stories that the day window is not thin.
            parsed("c1", "Sumqayıtda zavod yanğını söndürüldü", NOW - 600),
            parsed("c2", "Naxçıvanda yeni magistral yol açıldı", NOW - 1200),
            parsed("c3", "Astara bağçılıq məhsulları ixrac olunur", NOW - 1800),
        ];
        store.upsert_items(source_id, &current, NOW).unwrap();

        let mut app = App::new(store, config, NOW);
        app.refresh(NOW);

        let spanning = app
            .stories
            .iter()
            .find(|story| story.title.contains("metro"))
            .expect("the spanning story ranks in the current window")
            .key
            .clone();
        let deltas = app.deltas.as_ref().expect("a stored ranking is comparable");
        assert!(
            deltas.contains_key(&spanning),
            "the current ranking and the stored one must share one key:\n{spanning}"
        );

        // A story that only exists in the 7d window keeps its key across a switch.
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        store
            .upsert_items(
                source_id,
                &[parsed(
                    "week",
                    "Zaqatalada qoz bağı genişləndirilir",
                    NOW - 3 * DAY,
                )],
                NOW,
            )
            .unwrap();
        let mut app = App::new(store, Config::default(), NOW);
        app.refresh(NOW);
        app.switch_window(Window::Week);
        let week_key = app
            .stories
            .iter()
            .find(|story| story.title.contains("Zaqatala"))
            .map(|story| story.key.clone())
            .expect("the 7d-only story ranks in the week window");
        app.switch_window(Window::Day);
        app.switch_window(Window::Week);
        let again = app
            .stories
            .iter()
            .find(|story| story.title.contains("Zaqatala"))
            .map(|story| story.key.clone())
            .expect("the story comes back");
        assert_eq!(week_key, again);
    }

    /// A window with one or two stories says so. It is not the same statement as "no news", and
    /// the screen offers the wider period instead of taking it.
    #[test]
    fn a_thin_window_reports_its_own_count_and_offers_the_wider_one() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let items = vec![
            parsed(
                "a",
                "Bakıda metro stansiyasında təmir işləri başladı",
                NOW - 60,
            ),
            parsed(
                "b",
                "Gəncədə toy karvanı qəza etdi, yaralılar var",
                NOW - 120,
            ),
        ];
        store.upsert_items(source_id, &items, NOW).unwrap();
        let mut app = App::new(store, Config::default(), NOW);
        // The sentences asserted below are English, so the screen is set to English here.
        app.lang = Lang::En;
        app.window = Window::Hour;
        app.refresh(NOW);

        assert_eq!(app.window_total, 2, "the hour really holds two stories");
        assert_eq!(app.stories.len(), 2, "nothing is substituted for them");
        let screen = render_app(&app);
        assert!(screen.contains("2 stories"), "{screen}");
        assert!(screen.contains("in the last hour"), "{screen}");
        assert!(screen.contains("press w for"), "{screen}");
        assert!(
            !screen.contains("no stories"),
            "two stories are not nothing:\n{screen}"
        );
    }

    /// One result is still a result, and the message counts it.
    #[test]
    fn a_filter_that_leaves_one_story_does_not_call_the_window_empty() {
        let mut app = app_with_stories();
        app.window = Window::Hour;
        app.filter = "metro".into();
        app.refresh(NOW);
        assert_eq!(app.stories.len(), 1);
        let screen = render_app(&app);
        assert!(screen.contains("1 of"), "{screen}");
        assert!(!screen.contains("no stories"), "{screen}");
    }

    #[test]
    fn a_full_window_says_nothing_about_widening() {
        let mut app = app_with_stories();
        app.refresh(NOW);
        assert!(app.window_total >= crate::app::THIN_WINDOW);
        assert!(!render_app(&app).contains("press w for"));
    }

    /// I4: the filter narrows what is shown and nothing else. Movement is measured in the index,
    /// so a story's position does not change because another story was filtered out of the view.
    #[test]
    fn the_text_filter_does_not_move_a_story_in_the_index() {
        let mut store = Store::open_in_memory().unwrap();
        let rss = seed_source(&mut store);
        let titles = [
            "Bakıda metro stansiyasında təmir işləri başladı",
            "Bakıda avtobus xətti dəyişdirildi",
            "Bakıda park bağlarına baxıldı",
            "Şəkidə ipək emalatxanası genişləndirildi",
        ];
        // Yesterday's reports of each story, ingested yesterday.
        let yesterday: Vec<ParsedItem> = titles
            .iter()
            .enumerate()
            .map(|(index, title)| {
                parsed(
                    &format!("old{index}"),
                    title,
                    NOW - DAY - 300 * (index as i64 + 1),
                )
            })
            .collect();
        store.upsert_items(rss, &yesterday, NOW - DAY).unwrap();

        let config = Config::default();
        record_rankings(&mut store, &config, NOW - DAY).unwrap();

        // Today's reports of the same stories.
        let today: Vec<ParsedItem> = titles
            .iter()
            .enumerate()
            .map(|(index, title)| {
                parsed(
                    &format!("new{index}"),
                    title,
                    NOW - 300 * (index as i64 + 1),
                )
            })
            .collect();
        store.upsert_items(rss, &today, NOW).unwrap();

        let mut app = App::new(store, config, NOW);

        // The same data, filtered and unfiltered: the deltas are identical.
        app.refresh(NOW);
        let unfiltered = app.deltas.clone().expect("a stored ranking exists");
        let unfiltered_total = app.window_total;
        assert!(!unfiltered.is_empty());

        app.filter = "baki".to_string();
        app.refresh(NOW);
        assert_eq!(app.stories.len(), 3, "the matching stories survive");
        assert_eq!(
            app.window_total, unfiltered_total,
            "the filter does not change how many stories the window holds"
        );
        assert_eq!(app.window, Window::Day, "and it does not change the period");
        assert_eq!(
            app.deltas.as_ref(),
            Some(&unfiltered),
            "the index positions are the same whether or not the view is narrowed"
        );
    }

    /// Nothing stored means no comparison, so the column stays hidden rather than showing a row
    /// of dashes or a claim about a ranking that was never computed.
    #[test]
    fn without_a_stored_ranking_the_movement_column_is_hidden() {
        let mut app = app_with_stories();
        app.window = Window::Hour;
        app.refresh(NOW + 7200);
        assert!(app.stories.is_empty());
        assert!(app.deltas.is_none());
    }

    /// `Enter` opens the publication the marker is on, and the marker is the user's choice.
    #[test]
    fn enter_opens_the_chosen_publication_and_not_silently_the_first() {
        let mut store = Store::open_in_memory().unwrap();
        let first = seed_source(&mut store);
        let second = store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Rss,
                    outlet: "Report".into(),
                    name: "Report RSS".into(),
                    locator: "https://report.az/rss/".into(),
                },
                true,
            )
            .unwrap();
        let title = "Bakıda metro stansiyasında təmir işləri başladı";
        store
            .upsert_items(first, &[parsed("a", title, NOW - 60)], NOW)
            .unwrap();
        let mut other = parsed("b", title, NOW - 120);
        other.url = "https://report.az/news/1".into();
        store.upsert_items(second, &[other], NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        app.refresh(NOW);
        assert_eq!(app.stories[0].outlets.len(), 2);

        // The marker starts on the first outlet and says so; the chosen one is the one opened.
        assert_eq!(app.outlet, 0);
        match app.handle_key(key(KeyCode::Enter)) {
            Action::OpenUrl(url) => assert!(url.starts_with("https://apa.az/"), "{url}"),
            other => panic!("expected OpenUrl, got {other:?}"),
        }

        app.handle_key(key(KeyCode::Char(']')));
        assert_eq!(app.outlet, 1, "the marker moved to the second outlet");
        match app.handle_key(key(KeyCode::Enter)) {
            Action::OpenUrl(url) => assert!(url.starts_with("https://report.az/"), "{url}"),
            other => panic!("expected OpenUrl, got {other:?}"),
        }

        // At the end of the list the marker stays put rather than wrapping to the other one.
        app.handle_key(key(KeyCode::Char(']')));
        assert_eq!(app.outlet, 1);
        app.handle_key(key(KeyCode::Char('[')));
        assert_eq!(app.outlet, 0);
        app.handle_key(key(KeyCode::Char('[')));
        assert_eq!(
            app.outlet, 0,
            "no wrap-around: the marker is where it looks"
        );
    }

    /// A ranked story carries a lede, cut at three hundred characters, because a list row has
    /// room for nothing longer. The details view reads the whole stored text — and reads it when
    /// it opens, so the list and the filter never pay for a query they do not use.
    #[test]
    fn the_details_view_reads_the_whole_stored_text() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Rss,
                    outlet: "APA".into(),
                    name: "APA RSS".into(),
                    locator: "https://apa.az/rss".into(),
                },
                true,
            )
            .unwrap();
        let text = "Yolların təmiri sentyabrın sonuna qədər davam edəcək. ".repeat(40);
        let mut post = parsed("long", "Bakıda yollar bağlıdır", NOW - 60);
        post.description = Some(text.clone());
        store.upsert_items(source_id, &[post], NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        app.refresh(NOW);
        assert!(
            app.bodies.is_empty(),
            "a list screen reads no whole bodies: {}",
            app.bodies.len()
        );
        assert_eq!(
            app.stories[0]
                .description
                .as_deref()
                .map(str::chars)
                .map(Iterator::count),
            Some(281),
            "the ranked story holds a lede and an ellipsis"
        );

        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Details);
        let url = app.stories[0].outlets[0].url.clone();
        assert_eq!(
            app.bodies.get(&url).map(String::as_str),
            Some(text.as_str()),
            "the details view holds the text as the source published it"
        );
    }

    /// The details view is the way to the whole card on a short terminal, and it scrolls.
    #[test]
    fn details_mode_scrolls_the_whole_story_and_escape_returns() {
        let mut app = app_with_stories();
        app.refresh(NOW);
        app.handle_key(key(KeyCode::Char('d')));
        assert_eq!(app.mode, Mode::Details);

        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.scroll, 2, "j scrolls the details view");
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.scroll, 1);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.scroll, 11);
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.scroll, 0);
        app.handle_key(key(KeyCode::Char('G')));
        assert!(app.scroll > 0, "G reaches the end of the story");

        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None);
        assert_eq!(app.mode, Mode::List);
        assert_eq!(app.scroll, 0);
    }

    /// The sort changes the order and nothing else. Deltas are index movement, so they must read
    /// the same whatever order the list is drawn in.
    #[test]
    fn the_sort_orders_the_list_without_touching_a_score_or_a_delta() {
        let mut app = app_with_stories();
        app.refresh(NOW);
        let scores: Vec<f64> = app.stories.iter().map(|story| story.score).collect();
        assert_eq!(app.sort, Sort::Index);

        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.sort, Sort::Growth);
        assert!(
            app.status.contains("recent growth"),
            "the status names the order: {}",
            app.status
        );

        app.handle_key(key(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.sort, Sort::Views);
        assert!(
            app.stories
                .windows(2)
                .all(|pair| pair[0].view_count >= pair[1].view_count),
            "the view order is by views"
        );

        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.sort, Sort::Chronology);
        assert!(
            app.stories
                .windows(2)
                .all(|pair| pair[0].newest >= pair[1].newest),
            "the chronology order is newest first"
        );

        app.handle_key(key(KeyCode::Char('s')));
        assert_eq!(app.sort, Sort::Index, "the cycle comes back around");
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| b.total_cmp(a));
        let again: Vec<f64> = app.stories.iter().map(|story| story.score).collect();
        assert_eq!(again, sorted, "the index order and the scores are intact");
    }

    /// I7: a failed store read must be visible, not silent.
    #[test]
    fn a_failed_store_read_sets_a_visible_status() {
        let mut app = app_with_stories();
        app.store.conn.execute("DROP TABLE items", []).unwrap();
        app.refresh(NOW);
        assert!(!app.status.is_empty(), "{:?}", app.status);
    }

    /// I10: a degraded source stays named until it succeeds again.
    #[test]
    fn a_degraded_source_stays_listed_until_it_succeeds() {
        let mut app = app_with_stories();
        app.record_poll(
            &PollReport {
                failed: vec![("APA RSS".into(), "500".into())],
                ..Default::default()
            },
            NOW,
        );
        assert!(app.degraded.contains(&"APA RSS".to_string()));
        assert!(app.status.contains("APA RSS"));

        app.record_poll(
            &PollReport {
                ok: 1,
                succeeded: vec!["APA RSS".into()],
                ..Default::default()
            },
            NOW + 60,
        );
        assert!(app.degraded.is_empty());
        assert!(app.status.is_empty());
    }

    /// I12: a dead input thread must say so.
    #[test]
    fn an_input_failure_sets_the_status() {
        let mut app = app_with_stories();
        assert!(app.status.is_empty());
        app.handle(AppEvent::InputFailed("terminal closed".into()));
        assert!(app.status.contains("terminal closed"));
    }

    /// The status line is part of the screen. A degraded source named in English under a
    /// Russian header is the kind of half-translation the strings table exists to prevent.
    #[test]
    fn the_status_line_speaks_the_configured_language() {
        let mut app = app_with_stories();
        app.lang = Lang::Ru;
        app.record_poll(
            &PollReport {
                failed: vec![("APA RSS".into(), "500".into())],
                ..Default::default()
            },
            NOW,
        );
        assert!(
            app.status.contains("не отвечают источники"),
            "the status is in the screen's language: {}",
            app.status
        );
        // The source's own name is data, not chrome, and is never translated.
        assert!(app.status.contains("APA RSS"), "{}", app.status);

        app.handle(AppEvent::PollFailed("connection reset".into()));
        assert!(
            app.status.contains("опрос не удался: connection reset"),
            "the prefix is ours and translated, the error is the system's and kept: {}",
            app.status
        );
    }

    /// I2: the tick re-ranks after 60 seconds of injected time, not before.
    #[test]
    fn the_tick_refreshes_after_a_minute_of_injected_time_and_not_sooner() {
        let mut app = app_with_stories();
        app.window = Window::Hour;
        app.refresh(NOW);
        assert_eq!(app.window_total, 5);

        // 30 seconds later the hour still holds the same five stories, and nothing was re-ranked.
        app.tick(NOW + 30);
        assert_eq!(app.window_total, 5, "30s of injected time does not re-rank");

        // Two hours later the same tick does re-rank, and the hour is empty.
        app.tick(NOW + 7200);
        assert_eq!(app.window_total, 0, "the hour has gone by");
        assert!(app.stories.is_empty());

        // One second after that ranking, the throttle still holds.
        app.refresh(NOW + 7200);
        let at = app.window_total;
        app.tick(NOW + 7201);
        assert_eq!(
            app.window_total, at,
            "one second later the tick does not re-rank"
        );
    }

    /// One Telegram post for the synthetic fixtures.
    fn post(external_id: &str, title: &str, published_at: i64, views: i64) -> ParsedItem {
        ParsedItem {
            external_id: external_id.to_string(),
            url: format!("https://t.me/apa_az/{external_id}"),
            title: title.to_string(),
            description: None,
            section: None,
            published_at,
            views: Some(views),
            cited: false,
            cited_outlet: None,
            publisher: None,
        }
    }

    fn telegram_source(store: &mut Store) -> i64 {
        store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Telegram,
                    outlet: "APA".into(),
                    name: "APA Telegram".into(),
                    locator: "@apa_az".into(),
                },
                true,
            )
            .unwrap()
    }

    fn item_id(store: &Store, external_id: &str) -> i64 {
        store
            .conn
            .query_row(
                "SELECT id FROM items WHERE external_id = ?1",
                [external_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn insert_sample(store: &Store, item_id: i64, ts: i64, views: i64) {
        store
            .conn
            .execute(
                "INSERT INTO view_samples (item_id, ts, views) VALUES (?1, ?2, ?3)",
                rusqlite::params![item_id, ts, views],
            )
            .unwrap();
    }

    /// The filter matches any story in the window, not just the newest few. A match further down
    /// the list is still a match, and matching it does not change the period.
    #[test]
    fn the_filter_finds_a_match_further_down_the_list() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let headlines = [
            "Lənkəranda çay fabriki yenidən açıldı",
            "Şəkidə ipək emalatxanası genişləndirildi",
            "Qubada meşə yanğınına nəzarət edilir",
            "Astara bağçılıq məhsulları ixrac olunur",
            "Salyanda balıq emalı zavodu işə düşdü",
            "Naxçıvanda yeni magistral yol açıldı",
            "Gəncədə toxuculuq fabriki bərpa edilir",
            "Sumqayıtda kimya zavodu modernləşdirilir",
            "Mingəçevirdə su anbarının səviyyəsi artıb",
            "Şirvanda günəş elektrik stansiyası tikilir",
            "Zaqatalada qoz bağı genişləndirilir",
            "Qusarda turizm marşrutu açıldı",
            "Bakıda metro stansiyasında təmir işləri başladı",
            "Bakıda yeni park salınır",
        ];
        let items: Vec<ParsedItem> = headlines
            .iter()
            .enumerate()
            .map(|(index, title)| {
                parsed(
                    &format!("week{index}"),
                    title,
                    NOW - 3600 * (index as i64 + 1),
                )
            })
            .collect();
        store.upsert_items(source_id, &items, NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        // The week window, where all fourteen stories rank; the match is the second oldest.
        app.window = Window::Week;
        app.refresh(NOW);
        let unfiltered = app.stories.len();
        assert_eq!(unfiltered, 14);

        app.filter = "metro".into();
        app.refresh(NOW);

        assert_eq!(
            app.window,
            Window::Week,
            "the filter does not change the period"
        );
        assert_eq!(
            app.window_total, unfiltered,
            "and does not change what the window holds"
        );
        assert_eq!(app.stories.len(), 1, "the matching story is found");
        assert!(app.stories[0].title.contains("metro"));
    }

    /// A stored ranking is what it was. An article found later, a headline corrected later and a
    /// sample taken later all change today's rows, and none of them may change a ranking that
    /// was computed and written down before them.
    #[test]
    fn a_stored_ranking_is_not_rewritten_by_later_ingest_or_later_samples() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = telegram_source(&mut store);

        let plan = [
            ("Böyük yol qəzası baş verdi", NOW - 2 * DAY, 500),
            ("Kiçik kanal xəbəri gəldi", NOW - 2 * DAY + 60, 10),
            ("Orta xəbər belə oldu", NOW - 2 * DAY + 120, 20),
        ];
        for (index, (title, published, views)) in plan.iter().enumerate() {
            store
                .upsert_items(
                    source_id,
                    &[post(&format!("item{index}"), title, *published, *views)],
                    NOW - DAY,
                )
                .unwrap();
        }

        let config = Config::default();
        record_rankings(&mut store, &config, NOW - DAY).unwrap();
        let stored = store
            .snapshot_near(Window::Day, crate::score::ALGORITHM_VERSION, NOW - DAY, 0)
            .unwrap()
            .expect("the ranking was stored");
        let before = stored.stories.clone();
        assert!(!before.is_empty());

        // An article the app had never seen, published inside that ranking's window, plus a
        // headline rewritten in place and a view count that grew by a thousand times.
        store
            .upsert_items(
                source_id,
                &[post(
                    "late",
                    "Gəncədə toy karvanı qəza etdi, yaralılar var",
                    NOW - DAY - 3600,
                    10,
                )],
                NOW,
            )
            .unwrap();
        store
            .upsert_items(
                source_id,
                &[post(
                    "item0",
                    "Böyük yol qəzası baş verdi və yol bağlandı",
                    NOW - 2 * DAY,
                    500_000,
                )],
                NOW,
            )
            .unwrap();
        let id = item_id(&store, "item0");
        insert_sample(&store, id, NOW - 60, 500_000);

        let after = store
            .snapshot_near(Window::Day, crate::score::ALGORITHM_VERSION, NOW - DAY, 0)
            .unwrap()
            .expect("the ranking is still there")
            .stories;
        assert_eq!(
            before, after,
            "a stored ranking is a record of what was computed, not a recomputation"
        );
    }

    /// The movement column reads that stored ranking, and a story whose headline was rewritten
    /// is still the same story: the match is by word overlap, not by an exact key.
    #[test]
    fn a_rewritten_headline_matches_the_story_in_the_stored_ranking() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let mine = "Bakıda metro stansiyasında təmir işləri başladı";
        let yesterday = vec![
            parsed("story", mine, NOW - DAY - 600),
            parsed(
                "other",
                "Şəkidə ipək emalatxanası genişləndirildi",
                NOW - DAY - 900,
            ),
            parsed(
                "third",
                "Qubada meşə yanğınına nəzarət edilir",
                NOW - DAY - 1200,
            ),
        ];
        store
            .upsert_items(source_id, &yesterday, NOW - DAY)
            .unwrap();

        let config = Config::default();
        record_rankings(&mut store, &config, NOW - DAY).unwrap();

        // The feed rewrites its own headline, and reports the story again today.
        store
            .upsert_items(
                source_id,
                &[parsed(
                    "story",
                    "Bakıda metro stansiyasında təmir işləri davam edir",
                    NOW - DAY - 600,
                )],
                NOW,
            )
            .unwrap();
        store
            .upsert_items(
                source_id,
                &[parsed(
                    "story-today",
                    "Bakıda metro stansiyasında təmir işləri davam edir",
                    NOW - 600,
                )],
                NOW,
            )
            .unwrap();

        let mut app = App::new(store, config, NOW);
        app.refresh(NOW);
        let deltas = app.deltas.as_ref().expect("a stored ranking exists");
        let story = app
            .stories
            .iter()
            .find(|story| story.title.contains("metro"))
            .expect("the story still ranks");
        assert_eq!(
            deltas.get(&story.key),
            Some(&0),
            "the story held its place; a rewritten headline is not a new story:\n{deltas:?}"
        );
    }

    /// Channel paces for a past moment come from the rows and the measurements that existed
    /// then. The same posts keep being sampled, and later samples have moved the channel's pace
    /// since — they must not move the pace attributed to that moment.
    #[test]
    fn baselines_for_a_past_moment_ignore_later_samples_and_later_rows() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = telegram_source(&mut store);
        let now = NOW;
        let at = now - DAY - 1;

        // Twenty posts published before the moment, each gaining 100 views an hour by then.
        let posts: Vec<ParsedItem> = (0..20)
            .map(|index| post(&format!("post{index}"), "Əvvəlki xəbər", at - 100_000, 100))
            .collect();
        // Ingested at that moment, so the rows are ones the program had then.
        store.upsert_items(source_id, &posts, at).unwrap();
        for index in 0..20 {
            let id = item_id(&store, &format!("post{index}"));
            insert_sample(&store, id, at - 7200, 0);
            insert_sample(&store, id, at - 3600, 100);
            insert_sample(&store, id, at + 3600, 100_000);
        }

        assert_eq!(
            baselines_at(&store, at).unwrap().of(source_id),
            100.0,
            "the pace as of that moment, from the samples that existed then"
        );
        assert_eq!(
            baselines_at(&store, now).unwrap().of(source_id),
            100_000.0 / 3.0,
            "the pace today, which the sample taken afterwards did move"
        );

        // A row ingested after the moment is not part of what that moment could see.
        let mut later = Store::open_in_memory().unwrap();
        let source_id = telegram_source(&mut later);
        later.upsert_items(source_id, &posts, now).unwrap();
        for index in 0..20 {
            let id = item_id(&later, &format!("post{index}"));
            insert_sample(&later, id, at - 7200, 0);
            insert_sample(&later, id, at - 3600, 100);
        }
        assert!(
            baselines_at(&later, at).unwrap().is_fallback(source_id),
            "nothing was known at that moment, so the channel has no pace from it"
        );
    }

    /// A publication 3700 seconds ago and a repeat 60 seconds ago are one outlet carrying one
    /// story. Only the repeat is inside the hour window, and counting it as an arrival would
    /// invent a pickup that never happened.
    #[test]
    fn a_repeat_inside_the_window_is_not_a_new_pickup() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let title = "Uzun sürən hadisə davam edir";
        store
            .upsert_items(
                source_id,
                &[
                    parsed("original", title, NOW - 3700),
                    parsed("repeat", title, NOW - 60),
                ],
                NOW,
            )
            .unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        app.window = Window::Hour;
        app.refresh(NOW);

        assert_eq!(app.stories.len(), 1, "one story, one outlet");
        let story = &app.stories[0];
        assert_eq!(story.spread, 1, "the outlet carries it once");
        assert_eq!(
            story.spread_velocity, 0.0,
            "the outlet arrived before the window; a repeat is not a pickup"
        );
        assert_eq!(
            story.updated_at,
            NOW - 3700,
            "the story has not developed since its first report"
        );
        assert_eq!(
            story.newest,
            NOW - 60,
            "the newest item is still the repeat, and it is not what freshness measures"
        );
    }
}
