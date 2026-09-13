//! Rendering. Stateless: everything the frame needs arrives in `View`, so the same
//! function serves the real terminal and `TestBackend`.

use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::score::ScoredStory;
use crate::store::Window;

pub struct View<'a> {
    pub window: Window,
    pub stories: &'a [ScoredStory],
    pub selected: usize,
    /// `None` hides the delta column entirely, which is the state before any
    /// comparable history exists. A column of dashes teaches nothing.
    pub deltas: Option<&'a HashMap<String, i64>>,
    pub local_only: bool,
    pub filter: &'a str,
    pub quiet_fallback: bool,
    pub show_help: bool,
    pub sources_ok: usize,
    pub sources_total: usize,
    pub last_poll: Option<i64>,
    pub now: i64,
    pub status: &'a str,
}

fn relative(now: i64, then: i64) -> String {
    let delta = (now - then).max(0);
    match delta {
        0..=59 => format!("{delta}s ago"),
        60..=3599 => format!("{}m ago", delta / 60),
        3600..=86_399 => format!("{}h ago", delta / 3600),
        _ => format!("{}d ago", delta / 86_400),
    }
}

fn window_tabs(active: Window, counts: [usize; 3]) -> Line<'static> {
    let mut spans = Vec::new();
    for window in Window::all() {
        let index = match window {
            Window::Hour => 0,
            Window::Day => 1,
            Window::Week => 2,
        };
        let label = format!(" {} ({}) ", window.label(), counts[index]);
        let style = if window == active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

pub fn draw(frame: &mut Frame, view: &View<'_>) {
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(9),
    ])
    .split(frame.area());

    draw_header(frame, view, areas[0]);
    draw_list(frame, view, areas[1]);
    draw_detail(frame, view, areas[2]);
    if view.show_help {
        draw_help(frame, frame.area());
    }
}

const HELP_LINES: &[&str] = &[
    "1 / 2 / 3      switch window (1h, 24h, 7d)",
    "Tab            next window",
    "j / k, arrows  move selection",
    "g / G          jump to top / bottom",
    "Enter          open the article in the browser",
    "l              toggle the local filter",
    "/              filter by text, Esc clears",
    "r              poll now",
    "?              close this help",
    "q              quit",
];

fn draw_help(frame: &mut Frame, area: Rect) {
    let width = 56.min(area.width);
    let height = (HELP_LINES.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let lines: Vec<Line> = HELP_LINES.iter().map(|line| Line::from(*line)).collect();
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys ")),
        popup,
    );
}

fn draw_header(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let counts = [
        view.stories
            .iter()
            .filter(|_| view.window == Window::Hour)
            .count(),
        view.stories
            .iter()
            .filter(|_| view.window == Window::Day)
            .count(),
        view.stories
            .iter()
            .filter(|_| view.window == Window::Week)
            .count(),
    ];
    let health = format!("{}/{} sources ok", view.sources_ok, view.sources_total);
    let polled = view
        .last_poll
        .map(|ts| relative(view.now, ts))
        .unwrap_or_else(|| "never".to_string());
    let local = if view.local_only { " · LOCAL" } else { "" };
    let filter = if view.filter.is_empty() {
        String::new()
    } else {
        format!(" · /{}", view.filter)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " bakutrend — {health} · polled {polled}{local}{filter} "
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(window_tabs(view.window, counts)), inner);
}

fn draw_list(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let show_delta = view.deltas.is_some();
    let mut widths = vec![Constraint::Length(4)];
    if show_delta {
        widths.push(Constraint::Length(6));
    }
    widths.extend([
        Constraint::Length(7),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(7),
        Constraint::Min(20),
        Constraint::Length(8),
    ]);

    let mut header = vec![Cell::from("#")];
    if show_delta {
        header.push(Cell::from("Delta"));
    }
    header.extend(
        ["Score", "Cvg", "Eng", "Fresh", "Headline", "Outlets"]
            .into_iter()
            .map(Cell::from),
    );

    let rows: Vec<Row> = view
        .stories
        .iter()
        .enumerate()
        .map(|(index, story)| {
            let mut cells = vec![Cell::from(format!("{}", index + 1))];
            if let Some(deltas) = view.deltas {
                let delta = match deltas.get(&story.key) {
                    Some(movement) if *movement > 0 => format!("+{movement}"),
                    Some(movement) if *movement < 0 => format!("{movement}"),
                    Some(_) => "0".to_string(),
                    None => "new".to_string(),
                };
                cells.push(Cell::from(delta));
            }
            cells.extend([
                Cell::from(format!("{:.2}", story.score)),
                Cell::from(format!("{:.1}", story.coverage)),
                Cell::from(format!("{:.1}", story.engagement)),
                Cell::from(format!("{:.2}", story.freshness)),
                Cell::from(story.title.clone()),
                Cell::from(format!("{}", story.outlets.len())),
            ]);
            Row::new(cells)
        })
        .collect();

    let mut state = TableState::default();
    if !view.stories.is_empty() {
        state.select(Some(view.selected.min(view.stories.len() - 1)));
    }
    let table = Table::new(rows, widths)
        .header(Row::new(header).style(Style::default().add_modifier(Modifier::BOLD)))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL));
    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_detail(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" detail ");
    let Some(story) = view.stories.get(view.selected) else {
        frame.render_widget(
            Paragraph::new("No stories yet — the first poll is still running.").block(block),
            area,
        );
        return;
    };

    let mut lines = vec![
        Line::from(story.title.clone()),
        Line::from(format!(
            "coverage {:.1}   engagement {:.1}   freshness {:.2}   score {:.2}   views {}",
            story.coverage, story.engagement, story.freshness, story.score, story.view_count
        )),
    ];
    if view.quiet_fallback {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!(
                    "Quiet hour — showing the latest {} stories instead",
                    view.stories.len()
                ),
                Style::default().fg(Color::Yellow),
            )),
        );
    }
    for outlet in &story.outlets {
        let views = outlet
            .views
            .map(|v| format!("{v} views"))
            .unwrap_or_else(|| "—".to_string());
        lines.push(Line::from(format!(
            " {}  {}  {}  {}",
            outlet.outlet,
            relative(view.now, outlet.newest),
            views,
            outlet.url
        )));
    }
    if !view.status.is_empty() {
        lines.push(Line::from(Span::styled(
            view.status.to_string(),
            Style::default().fg(Color::Red),
        )));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::{OutletContribution, ScoredStory};
    use crate::store::Window;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn story(title: &str, coverage: f64) -> ScoredStory {
        ScoredStory {
            key: title.to_string(),
            title: title.to_string(),
            score: 0.9,
            coverage,
            coverage_norm: 1.0,
            engagement: 1.2,
            engagement_norm: 0.8,
            freshness: 0.7,
            view_count: 2650,
            newest: 0,
            outlets: vec![OutletContribution {
                outlet: "Qafqazinfo".into(),
                weight: 1.0,
                newest: 0,
                views: Some(2650),
                views_per_hour: Some(900.0),
                title: title.to_string(),
                url: "https://qafqazinfo.az/news/detail/x-1".into(),
            }],
        }
    }

    fn render(view: &View<'_>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal.draw(|frame| draw(frame, view)).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(|line| line.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn base<'a>(
        stories: &'a [ScoredStory],
        deltas: Option<&'a std::collections::HashMap<String, i64>>,
    ) -> View<'a> {
        View {
            window: Window::Day,
            stories,
            selected: 0,
            deltas,
            local_only: false,
            filter: "",
            quiet_fallback: false,
            show_help: false,
            sources_ok: 12,
            sources_total: 20,
            last_poll: Some(0),
            now: 0,
            status: "",
        }
    }

    #[test]
    fn header_reports_source_health_and_the_active_window() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let screen = render(&base(&stories, None));
        assert!(screen.contains("12/20 sources ok"), "{screen}");
        assert!(screen.contains("24h"), "{screen}");
        assert!(screen.contains("Bakıda bu yollar bağlıdır"), "{screen}");
    }

    #[test]
    fn the_delta_column_is_absent_until_comparable_history_exists() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let without = render(&base(&stories, None));
        assert!(
            !without.contains("Delta"),
            "no delta header without history:\n{without}"
        );

        let deltas =
            std::collections::HashMap::from([("Bakıda bu yollar bağlıdır".to_string(), 3i64)]);
        let with = render(&base(&stories, Some(&deltas)));
        assert!(
            with.contains("+3"),
            "a rising story shows its movement:\n{with}"
        );
    }

    #[test]
    fn the_quiet_fallback_is_labeled_and_never_silent() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let mut view = base(&stories, None);
        view.quiet_fallback = true;
        let screen = render(&view);
        assert!(screen.contains("Quiet hour"), "{screen}");
    }

    #[test]
    fn the_help_overlay_is_hidden_until_asked_for() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let mut view = base(&stories, None);
        assert!(!render(&view).contains("switch window"));

        view.show_help = true;
        let screen = render(&view);
        assert!(screen.contains("switch window"), "{screen}");
        assert!(screen.contains("poll now"), "{screen}");
    }

    #[test]
    fn the_detail_pane_shows_the_score_breakdown_and_every_outlet() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let screen = render(&base(&stories, None));
        assert!(screen.contains("coverage"), "{screen}");
        assert!(screen.contains("engagement"), "{screen}");
        assert!(screen.contains("freshness"), "{screen}");
        assert!(screen.contains("Qafqazinfo"), "{screen}");
    }

    #[test]
    fn a_very_long_headline_does_not_panic_or_overflow() {
        let long = "Bakıda ".repeat(200);
        let stories = vec![story(&long, 1.0)];
        let screen = render(&base(&stories, None));
        assert!(
            screen
                .lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 100)
        );
    }
}
