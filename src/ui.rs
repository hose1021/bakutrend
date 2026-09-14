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
    pub sources_ok: Option<usize>,
    pub sources_total: usize,
    pub last_poll: Option<i64>,
    /// Sources that failed and have not succeeded again (I10).
    pub degraded: &'a [String],
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

/// `View` only ever holds the stories of the active window, so no tab has a count the
/// header could honestly report. A number here would be a lie dressed as information.
fn window_tabs(active: Window) -> Line<'static> {
    let mut spans = Vec::new();
    for window in Window::all() {
        let style = if window == active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(format!(" {} ", window.label()), style));
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

pub fn draw(frame: &mut Frame, view: &View<'_>) {
    // One clamp for the whole frame: a refresh that shrinks the list must not leave the
    // table highlighting one row while the detail pane claims nothing is selected.
    let selected = view.selected.min(view.stories.len().saturating_sub(1));

    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(11),
    ])
    .split(frame.area());

    draw_header(frame, view, areas[0]);
    draw_list(frame, view, areas[1], selected);
    draw_detail(frame, view, areas[2], selected);
    if view.show_help {
        draw_help(frame, view, frame.area());
    }
}

fn draw_help(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let mut lines: Vec<Line> = HELP_LINES.iter().map(|line| Line::from(*line)).collect();
    if !view.degraded.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("degraded: {}", view.degraded.join(", ")),
            Style::default().fg(Color::Red),
        )));
    }
    let width = 56.min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" keys ")),
        popup,
    );
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

fn draw_header(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let health = match view.sources_ok {
        Some(ok) => format!("{ok}/{} sources ok", view.sources_total),
        // Health is unknown until a poll reports; claiming 0/0 or N/N would be a lie.
        None => "sources unknown".to_string(),
    };
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

    let block = Block::default().borders(Borders::ALL).title(format!(
        " bakutrend — {health} · polled {polled}{local}{filter} "
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(window_tabs(view.window)), inner);
}

fn draw_list(frame: &mut Frame, view: &View<'_>, area: Rect, selected: usize) {
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
        Constraint::Length(6),
        Constraint::Min(20),
        Constraint::Length(8),
    ]);

    let mut header = vec![Cell::from("#")];
    if show_delta {
        header.push(Cell::from("Delta"));
    }
    header.extend(
        ["Score", "Cvg", "Eng", "Fresh", "Vel", "Headline", "Outlets"]
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
                Cell::from(format!("{:.0}", story.spread_velocity)),
                Cell::from(story.title.clone()),
                Cell::from(format!("{}", story.outlets.len())),
            ]);
            Row::new(cells)
        })
        .collect();

    let mut state = TableState::default();
    if !view.stories.is_empty() {
        state.select(Some(selected));
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

/// The quiet-hour notice, so no branch can show a time range the header does not claim.
fn quiet_banner(view: &View<'_>) -> Line<'static> {
    Line::from(Span::styled(
        format!(
            "Quiet hour — showing the latest {} stories from the past week",
            view.stories.len()
        ),
        Style::default().fg(Color::Yellow),
    ))
}

fn status_line(view: &View<'_>) -> Option<Line<'static>> {
    (!view.status.is_empty()).then(|| {
        Line::from(Span::styled(
            view.status.to_string(),
            Style::default().fg(Color::Red),
        ))
    })
}

fn draw_detail(frame: &mut Frame, view: &View<'_>, area: Rect, selected: usize) {
    let block = Block::default().borders(Borders::ALL).title(" detail ");

    let mut lines = Vec::new();
    if view.quiet_fallback {
        lines.push(quiet_banner(view));
    }

    let Some(story) = view.stories.get(selected) else {
        // An empty list is not always good news: a failed poll or database read arrives
        // here as an error in `status`, and saying "still running" over it would hide the
        // one thing the user needs to see.
        // Only a run with NO poll yet may claim one is in progress (I5); a polled run
        // with an empty window says exactly that.
        let empty_message = if view.last_poll.is_none() {
            "No stories yet — the first poll is still running."
        } else {
            "No stories in this window since the last poll."
        };
        lines.push(Line::from(empty_message));
        lines.extend(status_line(view));
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    };

    lines.extend([
        Line::from(story.title.clone()),
        Line::from(format!(
            "score {:.2}   coverage {:.1} (norm {:.2})   engagement {:.1} (norm {:.2})   freshness {:.2}",
            story.score,
            story.coverage,
            story.coverage_norm,
            story.engagement,
            story.engagement_norm,
            story.freshness
        )),
        Line::from(format!(
            "spread {}   velocity {:.0} (norm {:.2})   started {}   updated {}   views {}",
            story.spread,
            story.spread_velocity,
            story.spread_velocity_norm,
            relative(view.now, story.started_at),
            relative(view.now, story.updated_at),
            story.view_count
        )),
    ]);
    for outlet in &story.outlets {
        let views = outlet
            .views
            .map(|v| format!("{v} views"))
            .unwrap_or_else(|| "—".to_string());
        let hourly = outlet
            .views_per_hour
            .map(|rate| format!("{rate:.0}/h"))
            .unwrap_or_else(|| "—".to_string());
        let velocity = outlet
            .relative_velocity
            .map(|ratio| format!("x{ratio:.1}"))
            .unwrap_or_else(|| "—".to_string());
        lines.push(Line::from(format!(
            " {}  {}  {}  {}  {}  {}  {}",
            outlet.outlet,
            outlet.provenance.label(),
            relative(view.now, outlet.newest),
            views,
            hourly,
            velocity,
            outlet.url
        )));
    }
    lines.extend(status_line(view));
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
            spread: 1,
            spread_velocity: 1.0,
            spread_velocity_norm: 1.0,
            started_at: 0,
            updated_at: 0,
            view_count: 2650,
            newest: 0,
            outlets: vec![OutletContribution {
                outlet: "Qafqazinfo".into(),
                weight: 1.0,
                provenance: crate::score::Provenance::Independent,
                newest: 0,
                views: Some(2650),
                views_per_hour: Some(900.0),
                relative_velocity: Some(2.0),
                title: title.to_string(),
                url: "https://qafqazinfo.az/news/detail/x-1".into(),
            }],
        }
    }

    /// Rebuild a row the way a terminal shows it. A wide grapheme occupies two cells and
    /// ratatui fills the cell it spills into with a space; that filler is not a column of
    /// its own, and counting it would overstate the line width.
    fn visible(line: &[ratatui::buffer::Cell]) -> String {
        let mut out = String::new();
        let mut filler = 0;
        for cell in line {
            if filler > 0 {
                filler -= 1;
                continue;
            }
            let symbol = cell.symbol();
            out.push_str(symbol);
            filler = unicode_width::UnicodeWidthStr::width(symbol).saturating_sub(1);
        }
        out
    }

    fn render(view: &View<'_>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal.draw(|frame| draw(frame, view)).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(visible)
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
            sources_ok: Some(12),
            filter: "",
            local_only: false,
            quiet_fallback: false,
            show_help: false,
            sources_total: 20,
            last_poll: Some(0),
            now: 0,
            degraded: &[],
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
    fn the_window_tabs_never_claim_a_count_they_cannot_know() {
        let stories: Vec<ScoredStory> = (0..5).map(|i| story(&format!("Xəbər {i}"), 1.0)).collect();
        let screen = render(&base(&stories, None));
        assert!(
            !screen.contains("(0)"),
            "a tab must not report a count it cannot know:\n{screen}"
        );
        for label in ["1h", "24h", "7d"] {
            assert!(
                screen.contains(label),
                "every window keeps its tab:\n{screen}"
            );
        }
    }

    #[test]
    fn a_stale_selection_does_not_split_the_list_and_the_detail_pane() {
        let stories = vec![story("Birinci xəbər", 1.0), story("İkinci xəbər", 5.0)];
        let mut view = base(&stories, None);
        view.selected = 99;
        let screen = render(&view);
        assert!(
            screen.contains("coverage 5.0"),
            "the detail pane must show the story the list highlights:\n{screen}"
        );
        assert!(!screen.contains("No stories yet"), "{screen}");

        let empty: Vec<ScoredStory> = Vec::new();
        assert!(render(&base(&empty, None)).contains("No stories"));
    }

    #[test]
    fn an_empty_list_still_surfaces_the_status() {
        let empty: Vec<ScoredStory> = Vec::new();
        let mut view = base(&empty, None);
        view.status = "database error: disk I/O error";
        let screen = render(&view);
        // `last_poll: Some(0)` means a poll HAS run: the pane must not claim one is
        // still running (I5). The status still surfaces.
        assert!(screen.contains("No stories in this window"), "{screen}");
        assert!(
            screen.contains("database error: disk I/O error"),
            "a failure must not hide behind a cheerful empty state:\n{screen}"
        );

        view.quiet_fallback = true;
        let screen = render(&view);
        assert!(screen.contains("Quiet hour"), "{screen}");
        assert!(
            screen.contains("database error: disk I/O error"),
            "{screen}"
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
        // Double-width characters and a unique tail: the tail proves the text was
        // truncated rather than silently spilling past the frame.
        let long = format!("{}SON", "Bakıda gözəl 世界 xəbər ".repeat(12));
        let stories = vec![story(&long, 1.0)];
        let screen = render(&base(&stories, None));
        assert!(
            screen.contains("Bakıda gözəl 世界"),
            "the headline must still render, wide characters included:\n{screen}"
        );
        assert!(
            !screen.contains("SON"),
            "the tail must be truncated, not spilled:\n{screen}"
        );
        assert!(
            screen
                .lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 100),
            "{screen}"
        );
    }
}
