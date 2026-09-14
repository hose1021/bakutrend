//! Application state. All mutable state lives here; `ui::draw` is stateless and
//! `main` is a thin composition root.

use std::collections::{HashMap, HashSet};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};

use crate::cluster::{Clusterer, Group};
use crate::config::Config;
use crate::poller::PollReport;
use crate::score::{ScoredStory, rank};
use crate::store::{ItemRow, Sample, Store, Window};
use crate::text::matches_any;
use crate::ui::View;

/// Below this many stories, the active window is treated as quiet and the list falls
/// back to the latest stories under a visible banner.
const QUIET_THRESHOLD: usize = 3;

/// The quiet fallback shows at most this many of the week's latest stories (I3).
const FALLBACK_LIMIT: usize = 12;

/// The tick re-ranks at most once per this many seconds of injected time (I2).
const REFRESH_INTERVAL_SECS: i64 = 60;

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
    pub stories: Vec<ScoredStory>,
    pub deltas: Option<HashMap<String, i64>>,
    pub selected: usize,
    pub local_only: bool,
    pub filter: String,
    pub filter_mode: bool,
    pub quiet_fallback: bool,
    pub show_help: bool,
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
        let (sources_total, enumeration_status) = match store.sources(true) {
            Ok(rows) => (
                rows.iter()
                    .filter(|row| row.kind != crate::source::SourceKind::Google)
                    .count(),
                String::new(),
            ),
            Err(error) => (0, format!("source list read failed: {error}")),
        };
        let mut app = Self {
            store,
            config,
            now,
            window: Window::Day,
            stories: Vec::new(),
            deltas: None,
            selected: 0,
            local_only: false,
            filter: String::new(),
            filter_mode: false,
            quiet_fallback: false,
            show_help: false,
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
    pub fn refresh(&mut self, now: i64) {
        self.now = now;
        let clusterer = Clusterer::new(self.config.cluster_threshold);
        let span = self.window.seconds();
        // One grouping over the full context range `[now - 2*span, now]`: a story's key
        // must not depend on which window is active, and the current and previous
        // rankings must share one identity (I1).
        let Ok((context, samples)) =
            self.store
                .window_data(now - 2 * span, now, self.window.allows_backfill())
        else {
            self.status = "database read failed".to_string();
            return;
        };
        let groups = clusterer.group_items(&context);

        self.quiet_fallback = false;
        let current = window_slice(&groups, now - span, now);
        let stories = self.apply_filters(
            &current,
            rank(&current, &samples, self.window, &self.config.weights, now),
        );

        // Spec §11: fewer than three stories is quiet — fall back to the latest stories
        // of the week, and let the banner state exactly what is shown (I3).
        if stories.len() < QUIET_THRESHOLD && self.window != Window::Week {
            let Ok((week_items, week_samples)) = self.store.window(Window::Week, now) else {
                self.status = "database read failed".to_string();
                self.deltas = None;
                self.stories = stories;
                self.selected = self.selected.min(self.stories.len().saturating_sub(1));
                return;
            };
            let week_groups = clusterer.group_items(&week_items);
            let latest = latest_groups(&week_groups, FALLBACK_LIMIT);
            let fallback = self.apply_filters(
                &latest,
                rank(
                    &latest,
                    &week_samples,
                    Window::Week,
                    &self.config.weights,
                    now,
                ),
            );
            self.quiet_fallback = true;
            self.deltas = None;
            self.stories = fallback;
            self.selected = self.selected.min(self.stories.len().saturating_sub(1));
            return;
        }

        self.deltas = self.compute_deltas(&groups, &samples, now, &stories);
        self.stories = stories;
        self.selected = self.selected.min(self.stories.len().saturating_sub(1));
    }

    /// The local filter matches the story title, each item's description and each outlet
    /// contribution's title (spec §10.1); the text filter matches the folded title.
    fn apply_filters(&self, groups: &[Group], mut stories: Vec<ScoredStory>) -> Vec<ScoredStory> {
        if self.local_only {
            let keys: HashSet<String> = groups
                .iter()
                .filter(|group| self.group_is_local(group))
                .map(|group| group.key.clone())
                .collect();
            stories.retain(|story| keys.contains(&story.key));
        }
        if !self.filter.is_empty() {
            let needle = crate::text::fold(&self.filter);
            stories.retain(|story| crate::text::fold(&story.title).contains(&needle));
        }
        stories
    }

    fn group_is_local(&self, group: &Group) -> bool {
        matches_any(&group.title, &self.config.local_keywords)
            || group.items.iter().any(|item| {
                matches_any(&item.title, &self.config.local_keywords)
                    || item
                        .description
                        .as_deref()
                        .is_some_and(|d| matches_any(d, &self.config.local_keywords))
            })
    }

    /// Express the change in position against the immediately preceding window, computed
    /// from the SAME grouping and the SAME filters as the current ranking (I4). `None`
    /// means there is nothing comparable, which hides the column.
    fn compute_deltas(
        &self,
        groups: &[Group],
        samples: &[Sample],
        now: i64,
        current: &[ScoredStory],
    ) -> Option<HashMap<String, i64>> {
        if current.is_empty() {
            return None;
        }
        let span = self.window.seconds();
        // The current window is `[now - span, now]`, so the previous one must end one
        // second earlier: an item published exactly on the boundary must not rank in
        // both windows. The off-by-one is deliberate.
        let previous_groups = window_slice(groups, now - 2 * span, now - span - 1);
        let previous_items: usize = previous_groups.iter().map(|g| g.items.len()).sum();
        if previous_items < QUIET_THRESHOLD {
            return None;
        }
        let previous = self.apply_filters(
            &previous_groups,
            rank(
                &previous_groups,
                samples,
                self.window,
                &self.config.weights,
                now - span,
            ),
        );
        if previous.is_empty() {
            return None;
        }

        let position: HashMap<&str, i64> = previous
            .iter()
            .enumerate()
            .map(|(index, story)| (story.key.as_str(), index as i64))
            .collect();
        Some(
            current
                .iter()
                .enumerate()
                .filter_map(|(index, story)| {
                    position
                        .get(story.key.as_str())
                        .map(|old_index| (story.key.clone(), old_index - index as i64))
                })
                .collect::<HashMap<_, _>>(),
        )
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
            format!(
                "{} source(s) degraded: {}",
                self.degraded.len(),
                self.degraded.join(", ")
            )
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
            window: self.window,
            stories: &self.stories,
            selected: self.selected,
            deltas: self.deltas.as_ref(),
            local_only: self.local_only,
            filter: &self.filter,
            quiet_fallback: self.quiet_fallback,
            show_help: self.show_help,
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
                self.status = self.compose_status(&message);
                Action::None
            }
            AppEvent::InputFailed(message) => {
                self.status = self.compose_status(&message);
                Action::None
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if self.filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filter_mode = false;
                    self.refresh(self.now);
                }
                KeyCode::Enter => self.filter_mode = false,
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
            return Action::None;
        }

        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc => {
                if self.show_help {
                    self.show_help = false;
                } else {
                    return Action::Quit;
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.selected + 1 < self.stories.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::Char('G') | KeyCode::End => {
                self.selected = self.stories.len().saturating_sub(1);
            }
            KeyCode::Char('1') => self.switch_window(Window::Hour),
            KeyCode::Char('2') => self.switch_window(Window::Day),
            KeyCode::Char('3') => self.switch_window(Window::Week),
            KeyCode::Tab => {
                let next = match self.window {
                    Window::Hour => Window::Day,
                    Window::Day => Window::Week,
                    Window::Week => Window::Hour,
                };
                self.switch_window(next);
            }
            KeyCode::Char('l') => {
                self.local_only = !self.local_only;
                self.refresh(self.now);
            }
            KeyCode::Char('/') => self.filter_mode = true,
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('r') => return Action::ForcePoll,
            KeyCode::Enter => {
                if let Some(story) = self.stories.get(self.selected)
                    && let Some(outlet) = story.outlets.first()
                {
                    return Action::OpenUrl(outlet.url.clone());
                }
            }
            _ => {}
        }
        Action::None
    }

    fn switch_window(&mut self, window: Window) {
        if self.window != window {
            self.window = window;
            self.selected = 0;
            self.refresh(self.now);
        }
    }
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

/// The `limit` groups holding the newest items — the quiet fallback's list (I3).
fn latest_groups(groups: &[Group], limit: usize) -> Vec<Group> {
    let mut ordered: Vec<&Group> = groups.iter().collect();
    ordered.sort_by_key(|group| std::cmp::Reverse(group.newest));
    ordered.into_iter().take(limit).cloned().collect()
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
                publisher: None,
            })
            .collect();
        store.upsert_items(source_id, &items, NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
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

    #[test]
    fn l_toggles_the_local_filter() {
        let mut app = app_with_stories();
        assert!(!app.local_only);
        app.handle_key(key(KeyCode::Char('l')));
        assert!(app.local_only);
        app.handle_key(key(KeyCode::Char('l')));
        assert!(!app.local_only);
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
    fn question_mark_toggles_help_and_escape_closes_it_before_quitting() {
        let mut app = app_with_stories();
        assert!(!app.show_help);
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.show_help);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            Action::None,
            "Esc closes help first"
        );
        assert!(!app.show_help);
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            Action::Quit,
            "then Esc quits"
        );
    }

    #[test]
    fn the_local_filter_keeps_only_matching_stories() {
        let mut app = app_with_stories();
        app.config.local_keywords = vec!["apa".to_string()];
        app.local_only = true;
        app.refresh(NOW);
        assert!(app.stories.is_empty(), "no headline mentions the keyword");
    }

    #[test]
    fn a_quiet_window_falls_back_to_the_latest_stories_with_a_label() {
        let mut app = app_with_stories();
        app.window = Window::Hour;
        // All five items are inside the hour, so nothing is quiet yet.
        app.refresh(NOW);
        assert!(!app.quiet_fallback);

        // Push the items outside the hour window.
        app.refresh(NOW + 7200);
        assert!(
            app.quiet_fallback,
            "an empty hour falls back rather than showing nothing"
        );
        assert!(
            !app.stories.is_empty(),
            "the fallback still shows the latest stories"
        );
    }

    #[test]
    fn refresh_groups_headlines_sharing_vocabulary_into_one_story() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let items = vec![
            parsed(
                "same-a",
                "Bakıda metro stansiyasında təmir işləri başladı",
                NOW,
            ),
            parsed(
                "same-b",
                "Bakıda metro stansiyasında təmir işləri davam edir",
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

    #[test]
    fn the_previous_window_excludes_an_item_on_the_current_window_boundary() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let boundary = "Astara bağçılıq məhsulları ixrac olunur";
        let mut items = vec![parsed("boundary", boundary, NOW - DAY)];
        for (index, title) in [
            "Lənkəranda çay fabriki yenidən açıldı",
            "Şəkidə ipək emalatxanası genişləndirildi",
            "Qubada meşə yanğınına nəzarət edilir",
        ]
        .iter()
        .enumerate()
        {
            items.push(parsed(
                &format!("prev{index}"),
                title,
                NOW - DAY - 600 * (index as i64 + 1),
            ));
        }
        // Keep the current window out of quiet fallback so deltas are computed.
        items.push(parsed(
            "cur1",
            "Sumqayıtda zavod yanğını söndürüldü",
            NOW - 600,
        ));
        items.push(parsed(
            "cur2",
            "Naxçıvanda yeni magistral yol açıldı",
            NOW - 1200,
        ));
        store.upsert_items(source_id, &items, NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        app.refresh(NOW);

        let boundary_key = app
            .stories
            .iter()
            .find(|story| story.title == boundary)
            .expect("the boundary item belongs to the current window")
            .key
            .clone();
        let deltas = app
            .deltas
            .as_ref()
            .expect("three prior items are enough history");
        assert!(
            !deltas.contains_key(&boundary_key),
            "an item published at exactly now - span must not rank in the previous window too"
        );
    }

    #[test]
    fn slash_filter_mode_narrows_the_list_and_escape_clears_it() {
        let mut app = app_with_stories();
        app.set_now(NOW);
        app.handle_key(key(KeyCode::Char('/')));
        assert!(app.filter_mode, "slash enters filter mode");

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
        assert!(!app.filter_mode, "Esc leaves filter mode");
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
    /// with the active window.
    #[test]
    fn a_story_spanning_both_windows_keeps_one_key_across_rankings() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let items = vec![
            parsed(
                "old",
                "Bakıda metro stansiyasında təmir işləri başladı",
                NOW - DAY - 3600,
            ),
            parsed(
                "new",
                "Bakıda metro stansiyasında təmir işləri davam edir",
                NOW - 3600,
            ),
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
            // Enough current-window stories that the day window is not quiet.
            parsed("c1", "Sumqayıtda zavod yanğını söndürüldü", NOW - 600),
            parsed("c2", "Naxçıvanda yeni magistral yol açıldı", NOW - 1200),
        ];
        store.upsert_items(source_id, &items, NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        app.refresh(NOW);

        let spanning = app
            .stories
            .iter()
            .find(|story| story.title.contains("metro"))
            .expect("the spanning story ranks in the current window")
            .key
            .clone();
        let deltas = app
            .deltas
            .as_ref()
            .expect("three prior items are enough history");
        assert!(
            deltas.contains_key(&spanning),
            "the current and previous rankings must share one key:\n{spanning}"
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

    /// I3: fewer than three stories falls back, and the banner states what is shown.
    #[test]
    fn a_window_with_two_stories_falls_back_and_labels_the_real_count() {
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
        app.window = Window::Hour;
        app.refresh(NOW);

        assert!(app.quiet_fallback);
        assert_eq!(app.stories.len(), 2, "the fallback shows what exists");
        let screen = render_app(&app);
        assert!(screen.contains("Quiet hour"), "{screen}");
        assert!(screen.contains("2 stories"), "{screen}");
        assert!(screen.contains("week"), "{screen}");
    }

    #[test]
    fn a_window_with_five_stories_does_not_fall_back() {
        let mut app = app_with_stories();
        app.refresh(NOW);
        assert!(!app.quiet_fallback);
        assert!(!render_app(&app).contains("Quiet hour"));
    }

    /// I4: the previous ranking uses the same grouping and the same filters, so a
    /// filtered-out story cannot distort the movement of the stories that remain.
    #[test]
    fn the_local_filter_keeps_the_previous_ranking_honest() {
        let mut store = Store::open_in_memory().unwrap();
        let rss = seed_source(&mut store);
        let second = store
            .ensure_source(
                &crate::source::SourceSpec {
                    kind: crate::source::SourceKind::Rss,
                    outlet: "Report".into(),
                    name: "Report RSS".into(),
                    locator: "https://report.az/rss/".into(),
                },
                true,
            )
            .unwrap();
        let items = vec![
            // A, B and C match the keyword and each spans both windows.
            parsed(
                "a-old",
                "Bakıda metro stansiyasında təmir işləri başladı",
                NOW - DAY - 300,
            ),
            parsed(
                "a-new",
                "Bakıda metro stansiyasında təmir işləri başladı",
                NOW - 300,
            ),
            parsed(
                "b-old",
                "Bakıda avtobus xətti dəyişdirildi",
                NOW - DAY - 600,
            ),
            parsed("b-new", "Bakıda avtobus xətti dəyişdirildi", NOW - 600),
            parsed("c-old", "Bakıda park bağlarına baxıldı", NOW - DAY - 900),
            parsed("c-new", "Bakıda park bağlarına baxıldı", NOW - 900),
            // D does not match the keyword.
            parsed(
                "d-old",
                "Şəkidə ipək emalatxanası genişləndirildi",
                NOW - DAY - 150,
            ),
            parsed(
                "d-new",
                "Şəkidə ipək emalatxanası genişləndirildi",
                NOW - 150,
            ),
        ];
        // D carries a second outlet, so unfiltered it outranks A, B and C in the
        // previous window and would distort their movement.
        let mut extra = parsed(
            "d-old-2",
            "Şəkidə ipək emalatxanası genişləndirildi",
            NOW - DAY - 150,
        );
        extra.url = "https://report.az/x".into();
        store.upsert_items(rss, &items, NOW).unwrap();
        store.upsert_items(second, &[extra], NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        app.config.local_keywords = vec!["Bakı".to_string()];
        app.local_only = true;
        app.refresh(NOW);

        let current_keys: Vec<&str> = app.stories.iter().map(|s| s.key.as_str()).collect();
        let deltas = app.deltas.as_ref().expect("three prior items are history");
        assert_eq!(current_keys.len(), 3, "the matching stories survive");
        for key in &current_keys {
            assert_eq!(
                deltas.get(*key),
                Some(&0),
                "the story's position must not move just because other stories were filtered:\n{deltas:?}"
            );
        }
    }

    #[test]
    fn the_fallback_hides_the_delta_column() {
        let mut app = app_with_stories();
        app.window = Window::Hour;
        app.refresh(NOW + 7200);
        assert!(app.quiet_fallback);
        assert!(app.deltas.is_none());
    }

    /// I6: the local filter matches item descriptions, not only titles.
    #[test]
    fn the_local_filter_matches_item_descriptions() {
        let mut store = Store::open_in_memory().unwrap();
        let source_id = seed_source(&mut store);
        let mut item = parsed("desc", "İclas keçirildi", NOW - 60);
        item.description = Some("Bakıda keçirildi".into());
        store.upsert_items(source_id, &[item], NOW).unwrap();

        let mut app = App::new(store, Config::default(), NOW);
        app.local_only = true;
        app.refresh(NOW);
        assert!(
            !app.stories.is_empty(),
            "the description carries the local relevance"
        );
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

    /// I2: the tick re-ranks after 60 seconds of injected time, not before.
    #[test]
    fn the_tick_refreshes_after_a_minute_of_injected_time_and_not_sooner() {
        let mut app = app_with_stories();
        app.window = Window::Hour;
        app.refresh(NOW);
        assert!(!app.quiet_fallback);

        app.tick(NOW + 30);
        assert!(!app.quiet_fallback, "30s of injected time does not re-rank");

        app.tick(NOW + 7200);
        assert!(
            app.quiet_fallback,
            "an advanced tick re-ranked the empty hour"
        );
        // Simulate staleness that only a refresh would repair.
        app.quiet_fallback = false;
        app.tick(NOW + 7201);
        assert!(
            !app.quiet_fallback,
            "one second later the tick does not re-rank"
        );
    }
}
