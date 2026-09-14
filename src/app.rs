//! Application state. All mutable state lives here; `ui::draw` is stateless and
//! `main` is a thin composition root.

use std::collections::HashMap;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};

use crate::cluster::Clusterer;
use crate::config::Config;
use crate::poller::PollReport;
use crate::score::{rank, ScoredStory};
use crate::store::{Store, Window};
use crate::text::matches_any;
use crate::ui::View;

/// Below this many stories, the active window is treated as quiet and the list falls
/// back to the latest stories under a visible banner.
const QUIET_THRESHOLD: usize = 3;

#[derive(Debug)]
pub enum AppEvent {
    Input(Event),
    PollDone(PollReport),
    PollFailed(String),
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
    pub window: Window,
    pub stories: Vec<ScoredStory>,
    pub deltas: Option<HashMap<String, i64>>,
    pub selected: usize,
    pub local_only: bool,
    pub filter: String,
    pub filter_mode: bool,
    pub quiet_fallback: bool,
    pub show_help: bool,
    pub sources_ok: usize,
    pub sources_total: usize,
    pub last_poll: Option<i64>,
    pub status: String,
}

impl App {
    pub fn new(store: Store, config: Config, now: i64) -> Self {
        // The Google seed is not a polled source, so it does not count toward health.
        let sources_total = store
            .sources(true)
            .map(|rows| {
                rows.iter()
                    .filter(|row| row.kind != crate::source::SourceKind::Google)
                    .count()
            })
            .unwrap_or(0);
        let mut app = Self {
            store,
            config,
            window: Window::Day,
            stories: Vec::new(),
            deltas: None,
            selected: 0,
            local_only: false,
            filter: String::new(),
            filter_mode: false,
            quiet_fallback: false,
            show_help: false,
            sources_ok: 0,
            sources_total,
            last_poll: None,
            status: String::new(),
        };
        app.sources_ok = app.sources_total;
        app.refresh(now);
        app
    }

    /// Regroup and rerank. Called when data or the window changes, not on every tick.
    pub fn refresh(&mut self, now: i64) {
        let clusterer = Clusterer::new(self.config.cluster_threshold);
        let Ok((items, samples)) = self.store.window(self.window, now) else {
            self.status = "database read failed".to_string();
            return;
        };

        let mut stories = if items.is_empty() && self.window != Window::Week {
            self.quiet_fallback = true;
            let Ok((fallback, fallback_samples)) = self.store.window(Window::Week, now) else {
                return;
            };
            let groups = clusterer.group_items(&fallback);
            rank(&groups, &fallback_samples, Window::Week, &self.config.weights, now)
        } else {
            self.quiet_fallback = false;
            let groups = clusterer.group_items(&items);
            rank(&groups, &samples, self.window, &self.config.weights, now)
        };

        if self.local_only {
            stories.retain(|story| {
                matches_any(&story.title, &self.config.local_keywords)
                    || story.outlets.iter().any(|o| matches_any(&o.title, &self.config.local_keywords))
            });
        }
        if !self.filter.is_empty() {
            let needle = crate::text::fold(&self.filter);
            stories.retain(|story| crate::text::fold(&story.title).contains(&needle));
        }
        self.quiet_fallback = self.quiet_fallback || stories.len() < QUIET_THRESHOLD;

        self.deltas = self.compute_deltas(&clusterer, now, &stories);
        self.stories = stories;
        self.selected = self.selected.min(self.stories.len().saturating_sub(1));
    }

    /// Rank the immediately preceding window of equal length and express the change in
    /// position. `None` means there is not enough history yet, which hides the column.
    fn compute_deltas(
        &self,
        clusterer: &Clusterer,
        now: i64,
        current: &[ScoredStory],
    ) -> Option<HashMap<String, i64>> {
        if current.is_empty() {
            return None;
        }
        let span = self.window.seconds();
        let Ok((previous_items, previous_samples)) =
            self.store.window_data(now - 2 * span, now - span, self.window.allows_backfill())
        else {
            return None;
        };
        if previous_items.len() < QUIET_THRESHOLD {
            return None;
        }
        let groups = clusterer.group_items(&previous_items);
        let previous = rank(&groups, &previous_samples, self.window, &self.config.weights, now - span);

        let position: HashMap<&str, i64> =
            previous.iter().enumerate().map(|(index, story)| (story.key.as_str(), index as i64)).collect();
        let deltas = current
            .iter()
            .enumerate()
            .filter_map(|(index, story)| {
                position
                    .get(story.key.as_str())
                    .map(|old_index| (story.key.clone(), old_index - index as i64))
            })
            .collect::<HashMap<_, _>>();
        Some(deltas)
    }

    pub fn record_poll(&mut self, report: &PollReport, now: i64) {
        self.last_poll = Some(now);
        self.sources_ok = report.ok;
        self.status = if report.failed.is_empty() {
            String::new()
        } else {
            format!(
                "{} source(s) degraded: {}",
                report.failed.len(),
                report
                    .failed
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        self.refresh(now);
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
            now: chrono::Utc::now().timestamp(),
            status: &self.status,
        }
    }

    pub fn handle(&mut self, event: AppEvent) -> Action {
        match event {
            AppEvent::Input(Event::Key(key)) => self.handle_key(key),
            AppEvent::Input(Event::Resize(..)) => Action::None,
            AppEvent::Input(_) => Action::None,
            AppEvent::PollDone(report) => {
                let now = chrono::Utc::now().timestamp();
                self.record_poll(&report, now);
                Action::None
            }
            AppEvent::PollFailed(message) => {
                self.status = message;
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
                    self.refresh(chrono::Utc::now().timestamp());
                }
                KeyCode::Enter => self.filter_mode = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refresh(chrono::Utc::now().timestamp());
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.refresh(chrono::Utc::now().timestamp());
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
                self.refresh(chrono::Utc::now().timestamp());
            }
            KeyCode::Char('/') => self.filter_mode = true,
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('r') => return Action::ForcePoll,
            KeyCode::Enter => {
                if let Some(story) = self.stories.get(self.selected) {
                    if let Some(outlet) = story.outlets.first() {
                        return Action::OpenUrl(outlet.url.clone());
                    }
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
            self.refresh(chrono::Utc::now().timestamp());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{ParsedItem, SourceKind, SourceSpec};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    const NOW: i64 = 1_700_000_000;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
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
        assert_eq!(app.selected, app.stories.len() - 1, "cannot move past the last row");
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
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::None, "Esc closes help first");
        assert!(!app.show_help);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), Action::Quit, "then Esc quits");
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
        assert!(app.quiet_fallback, "an empty hour falls back rather than showing nothing");
        assert!(!app.stories.is_empty(), "the fallback still shows the latest stories");
    }
}
