//! Rendering. Stateless: everything the frame needs arrives in `View`, so the same function
//! serves the real terminal and `TestBackend`.
//!
//! The screen is a reading list. The header says how fresh the data is and which window is on
//! screen, the list ranks the stories, the card explains the selected one, and the footer shows
//! the keys of the mode the user is in. Colour carries meaning only where there is meaning to
//! carry: cyan marks the score and the selection, green marks a story that was not in the
//! previous ranking, yellow warns that the screen is not showing what its tab claims, red
//! reports a failure.

use std::cmp::Ordering;
use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use unicode_width::UnicodeWidthStr;

use crate::app::{Mode, Sort};
use crate::i18n::{Keycap, Lang, Strings};
use crate::score::{OutletContribution, Provenance, ScoredStory, VelocityBasis};
use crate::store::Window;

/// Cyan: the score bar, the selected row's marker and the active tab.
const ACCENT: Color = Color::Cyan;
/// Green: a story that was not in the previous ranking. `NEW` is the one column that is not a
/// number, and green says it without a word.
const NEW: Color = Color::Green;
/// Yellow: the screen is showing something other than what its tab claims.
const WARN: Color = Color::Yellow;
/// Red: this program could not do what it was asked to do.
const FAILURE: Color = Color::Red;
/// Secondary text: labels, ranks and the numbers the headline does not carry.
const SECONDARY: Color = Color::DarkGray;
/// The divider between the list and the card.
const RULE: Color = Color::DarkGray;

/// The selected row.
///
/// Reversed video, not a fixed background colour. An indexed background assumes a dark terminal:
/// on a light theme the same index is a pale grey, and the selected row becomes the one row that
/// is harder to read than the others. Reversal swaps whatever the terminal is already using, so
/// it holds in both themes, and the accent marker in its own column carries the selection even
/// where a terminal renders bold and reverse as no change at all.
fn picked() -> Style {
    Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

/// Above this width the card sits beside the list. Below it a card is narrower than the numbers
/// it prints, and a value wrapped mid-word is worse than a card underneath.
const SIDE_BY_SIDE: u16 = 92;

/// The list's share of a stacked screen. A fixed-height card would leave a twenty-row terminal
/// with no stories at all, and the ranked list is what the screen is for; when the terminal is
/// tall enough for the card's whole height the split stops mattering.
const STACKED_LIST: u16 = 60;

/// The card's head shows the headline and up to this many lines of description.
const DESCRIPTION_LINES: usize = 3;

/// The bar's width in the card's signal rows.
const SIGNAL_BAR: usize = 6;

/// The signal rows' label column: the widest label is `Engagement`.
const SIGNAL_LABEL: usize = 11;

pub struct View<'a> {
    pub lang: Lang,
    pub window: Window,
    pub stories: &'a [ScoredStory],
    /// Full stored text of the selected story's items, keyed by URL. Empty until the details view
    /// asks for it, and the view falls back to the story's lede when a URL is absent.
    pub bodies: &'a HashMap<String, String>,
    /// Stories the window holds before the text filter: what the filter is narrowing, so the
    /// screen can say "2 of 14" instead of implying the window is empty.
    pub window_total: usize,
    pub selected: usize,
    /// Which outlet of the selected story `Enter` will open, marked in its list.
    pub outlet: usize,
    /// `None` hides every movement claim, which is the state before a comparable stored ranking
    /// exists. A column of dashes teaches nothing.
    pub deltas: Option<&'a HashMap<String, i64>>,
    /// When the stored ranking behind `deltas` was computed.
    pub comparison_at: Option<i64>,
    pub filter: &'a str,
    /// Where the keyboard is. Every key means one thing at a time, and the footer says which.
    pub mode: Mode,
    pub sort: Sort,
    /// First line shown by the details view, and by the help overlay.
    pub scroll: u16,
    pub help_scroll: u16,
    pub sources_ok: Option<usize>,
    pub sources_total: usize,
    pub last_poll: Option<i64>,
    /// Sources that failed and have not succeeded again (I10).
    pub degraded: &'a [String],
    pub now: i64,
    pub status: &'a str,
}

impl View<'_> {
    fn text(&self) -> &'static Strings {
        self.lang.strings()
    }
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn strong() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn secondary() -> Style {
    Style::default().fg(SECONDARY)
}

/// Truncate to display columns. A wide glyph is two columns, so a Cyrillic headline and one
/// with emoji do not measure the same in characters.
fn clip(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for glyph in text.chars() {
        let glyph_width = UnicodeWidthStr::width(glyph.to_string().as_str());
        if used + glyph_width >= width {
            break;
        }
        out.push(glyph);
        used += glyph_width;
    }
    out.push('…');
    out
}

/// Greedy wrap to display columns. The card's line count has to be exact: the block under it is
/// fixed, and a wrapped line nobody counted pushes the sources out of the box.
fn wrap(text: &str, width: usize, cap: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let word = clip(word, width);
        let candidate = if current.is_empty() {
            word.clone()
        } else {
            format!("{current} {word}")
        };
        if UnicodeWidthStr::width(candidate.as_str()) > width && !current.is_empty() {
            lines.push(current);
            current = word;
        } else {
            current = candidate;
        }
        if lines.len() == cap {
            return lines;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// A bar of a real normalised value, split so the caller can colour the value without colouring
/// the track.
fn bar(value: f64, width: usize) -> (String, String) {
    let filled = ((value.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    ("█".repeat(filled), "░".repeat(width - filled))
}

pub fn draw(frame: &mut Frame, view: &View<'_>) {
    // One clamp for the whole frame: a refresh that shrinks the list must not leave the
    // table highlighting one row while the card claims nothing is selected.
    let selected = view.selected.min(view.stories.len().saturating_sub(1));
    let status = notice(view, &view.text().widen(view.window.wider()));

    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(u16::from(status.is_some())),
        Constraint::Min(6),
        Constraint::Length(u16::from(!view.status.is_empty())),
        Constraint::Length(1),
    ])
    .split(frame.area());

    draw_header(frame, view, areas[0]);
    if let Some((text, level)) = &status {
        let colour = match level {
            Level::Plain => Color::Reset,
            Level::Warn => WARN,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                clip(text, areas[1].width as usize),
                Style::default().fg(colour),
            )),
            areas[1],
        );
    }
    // The details view replaces the list rather than sitting beside it: on a short terminal it
    // is the only way the signals and the whole source list fit, and it is the mode the footer
    // names while it is open.
    if view.mode == Mode::Details {
        draw_details(frame, view, areas[2], selected);
    } else {
        draw_body(frame, view, areas[2], selected);
    }
    // The status gets its own row rather than the card's tail: a failure cut off by a small
    // terminal is the one line the user cannot do without.
    if !view.status.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                clip(view.status, areas[3].width as usize),
                Style::default().fg(FAILURE),
            )),
            areas[3],
        );
    }
    draw_footer(frame, view, areas[4]);
    if view.mode == Mode::Help {
        draw_help(frame, view, frame.area());
    }
}

/// How loudly a notice speaks. The level is not a colour: the terminal paints the line and the
/// web page styles it, and both show the same sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    /// A count, or a filter that narrows the list.
    Plain,
    /// The screen is showing something other than what its tab claims.
    Warn,
}

/// The one notice line above the list, or `None` when there is nothing to say.
///
/// Every message counts what is actually on screen. "Nothing new" and "one story" are different
/// facts, and a screen that reports the first when the second is true is wrong about its own
/// contents. `hint` is what this front end can offer for a wider window when the period is thin:
/// a terminal names the key, a page has nothing to name, and neither widens the period by itself.
pub(crate) fn notice(view: &View<'_>, hint: &str) -> Option<(String, Level)> {
    let t = view.text();
    let shown = view.stories.len();
    let window = t.window(view.window);
    let hint = if hint.is_empty() || view.window == Window::Week {
        String::new()
    } else {
        format!(" · {hint}")
    };
    if !view.filter.is_empty() {
        if shown == 0 {
            return Some((
                format!(
                    "{}{hint}",
                    t.no_match(view.filter, view.window_total, window)
                ),
                Level::Warn,
            ));
        }
        return Some((t.matching(shown, view.window_total, window), Level::Plain));
    }
    if shown == 0 {
        return Some((
            format!("{}{hint}", t.empty_window(view.window)),
            Level::Warn,
        ));
    }
    if view.window_total < crate::app::THIN_WINDOW && view.window != Window::Week {
        return Some((
            format!("{}{hint}", t.thin_window(view.window_total, view.window)),
            Level::Warn,
        ));
    }
    None
}

/// Two rows and a rule: the program's name with the count of what it is showing, then the
/// window tabs with what is behind them.
fn draw_header(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    let t = view.text();
    let count = match view.window_total {
        0 => t.stories(view.stories.len()),
        total if total == view.stories.len() => t.stories(total),
        // The filter is narrowing the window, and the two numbers say so.
        total => t.matching(view.stories.len(), total, t.window(view.window)),
    };
    let count_width = UnicodeWidthStr::width(count.as_str()) as u16;

    // The active order is named next to the count whenever it is not the index: a list sorted by
    // views looks like a list of news until the screen says otherwise.
    let sort = if view.sort == Sort::Index {
        String::new()
    } else {
        format!("  ·  {}", t.sorted(t.sort(view.sort)))
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " bakutrend",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(sort, secondary()),
        ])),
        Rect {
            width: area.width.saturating_sub(count_width),
            ..rows[0]
        },
    );
    frame.render_widget(
        Paragraph::new(Span::styled(count, secondary())).alignment(Alignment::Right),
        Rect {
            x: rows[0].x + area.width.saturating_sub(count_width),
            width: count_width,
            ..rows[0]
        },
    );

    draw_tabs(frame, view, rows[1]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "─".repeat(rows[2].width as usize),
            secondary(),
        )),
        rows[2],
    );
}

fn draw_tabs(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let t = view.text();
    let mut spans = Vec::new();
    for window in Window::all() {
        // The active tab is reversed as well as coloured. Colour alone is not a signal: a
        // terminal with a palette that hides cyan, or a reader who cannot see the difference,
        // would have nothing to go on.
        let style = if window == view.window {
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            secondary()
        };
        spans.push(Span::styled(format!("[ {} ]", t.window(window)), style));
        spans.push(Span::raw(" "));
    }

    // The tabs say which window is on screen; the right edge says how much of the data behind
    // it this program actually has, so the two claims are read together.
    let mut right = String::new();
    if !view.filter.is_empty() {
        right.push_str(&format!("/{} · ", view.filter));
    }
    right.push_str(&t.health(view.sources_ok, view.sources_total));
    right.push_str(" · ");
    right.push_str(&match view.last_poll {
        Some(ts) => t.polled((view.now - ts).max(0)),
        None => format!("{} {}", t.polled, t.never),
    });
    let right = clip(
        &right,
        area.width
            .saturating_sub(tabs_width(view))
            .saturating_sub(1) as usize,
    );

    let tabs = Rect {
        width: area
            .width
            .saturating_sub(UnicodeWidthStr::width(right.as_str()) as u16),
        ..area
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), tabs);
    frame.render_widget(
        Paragraph::new(Span::styled(right, dim())).alignment(Alignment::Right),
        Rect {
            x: area.x + tabs.width,
            width: area.width - tabs.width,
            ..area
        },
    );
}

fn tabs_width(view: &View<'_>) -> u16 {
    let t = view.text();
    Window::all()
        .iter()
        .map(|window| UnicodeWidthStr::width(t.window(*window)) as u16 + 5)
        .sum()
}

/// The list and the card, side by side or stacked.
fn draw_body(frame: &mut Frame, view: &View<'_>, area: Rect, selected: usize) {
    if area.width >= SIDE_BY_SIDE {
        let parts = Layout::horizontal([
            Constraint::Percentage(62),
            Constraint::Length(1),
            Constraint::Min(30),
        ])
        .split(area);
        draw_list(frame, view, parts[0], selected);
        draw_rule(frame, parts[1]);
        draw_card(frame, view, parts[2], selected);
    } else {
        let parts = Layout::vertical([
            Constraint::Percentage(STACKED_LIST),
            Constraint::Percentage(100 - STACKED_LIST),
        ])
        .split(area);
        draw_list(frame, view, parts[0], selected);
        draw_card(frame, view, parts[1], selected);
    }
}

/// The vertical divider. A rule on its own, not a border around the card: two boxes side by
/// side are two frames to read instead of one screen.
fn draw_rule(frame: &mut Frame, area: Rect) {
    let column: Vec<Line> = (0..area.height)
        .map(|_| Line::from(Span::styled("│", Style::default().fg(RULE))))
        .collect();
    frame.render_widget(Paragraph::new(column), area);
}

fn draw_list(frame: &mut Frame, view: &View<'_>, area: Rect, selected: usize) {
    let t = view.text();
    let show_status = view.deltas.is_some();
    // The last column shows what the active order is about: a list sorted by views with an age
    // column would explain nothing about why the rows are in that order.
    let last_header = match view.sort {
        Sort::Growth => t.col_growth,
        Sort::Views => t.col_views,
        _ => t.col_age,
    };

    let mut columns = vec![
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(12),
        Constraint::Length(7),
    ];
    if show_status {
        columns.push(Constraint::Length(6));
    }
    columns.push(Constraint::Length(7));

    let mut header = vec![
        Cell::from(""),
        Cell::from(t.col_rank),
        Cell::from(t.col_headline),
        Cell::from(t.col_outlets),
    ];
    if show_status {
        header.push(Cell::from(t.col_status));
    }
    header.push(Cell::from(last_header));

    // The fixed columns and the spacing between them, so a clipped headline leaves the numbers
    // their room instead of pushing them off the edge.
    let fixed = 1 + 2 + 7 + 7 + if show_status { 6 } else { 0 } + columns.len() - 1;
    let room = (area.width as usize).saturating_sub(fixed);

    let rows: Vec<Row> = view
        .stories
        .iter()
        .enumerate()
        .map(|(index, story)| {
            let mut cells = vec![
                Cell::from(Span::styled(
                    if index == selected { "▌" } else { " " },
                    Style::default().fg(ACCENT),
                )),
                Cell::from(Span::styled(format!("{:02}", index + 1), secondary())),
                Cell::from(clip(&story.title, room)),
                Cell::from(Span::styled(story.outlets.len().to_string(), secondary())),
            ];
            if let Some(deltas) = view.deltas {
                // Movement of a rank, not of a score, and the sign carries it without colour.
                let (text, style) = match deltas.get(&story.key) {
                    Some(movement) if *movement > 0 => {
                        (format!("+{movement}"), Style::default().fg(ACCENT))
                    }
                    Some(movement) if *movement < 0 => (format!("{movement}"), Style::default()),
                    Some(_) => ("0".to_string(), secondary()),
                    None => (
                        t.new_label.to_string(),
                        Style::default().fg(NEW).add_modifier(Modifier::BOLD),
                    ),
                };
                cells.push(Cell::from(Span::styled(text, style)));
            }
            cells.push(Cell::from(Span::styled(
                match view.sort {
                    Sort::Growth => t.growth(best_relative(story)),
                    Sort::Views => compact(story.view_count),
                    _ => t.age((view.now - story.updated_at).max(0)),
                },
                secondary(),
            )));
            Row::new(cells)
        })
        .collect();

    let mut state = TableState::default();
    if !view.stories.is_empty() {
        state.select(Some(selected));
    }
    let table = Table::new(rows, columns)
        .column_spacing(1)
        .header(Row::new(header).style(secondary()))
        .row_highlight_style(picked())
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(secondary()),
        );
    frame.render_stateful_widget(table, area, &mut state);
}

/// The selected story's card: what the headline does not say.
fn draw_card(frame: &mut Frame, view: &View<'_>, area: Rect, selected: usize) {
    let t = view.text();
    // One column of padding, so the text does not touch the divider.
    let inner = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        ..area
    };
    let width = inner.width as usize;

    let mut head = Vec::new();
    match view.stories.get(selected) {
        Some(story) => {
            head.push(Line::from(Span::styled(
                clip(&story.title, width),
                strong(),
            )));
            if let Some(description) = &story.description {
                for line in wrap(description, width, DESCRIPTION_LINES) {
                    head.push(Line::from(Span::styled(line, dim())));
                }
            }
        }
        None => {
            // An empty list is not always good news: status carries any failure. Only a run
            // that has not polled yet may claim a poll is in progress (I5).
            let message = if view.last_poll.is_none() {
                t.empty_first_run.to_string()
            } else {
                t.empty_window_long.to_string()
            };
            head.push(Line::from(Span::styled(clip(&message, width), dim())));
        }
    }

    let parts =
        Layout::vertical([Constraint::Length(head.len() as u16), Constraint::Min(0)]).split(inner);
    frame.render_widget(Paragraph::new(head), parts[0]);

    if let Some(story) = view.stories.get(selected) {
        frame.render_widget(
            Paragraph::new(section_lines(view, story, width, false)),
            parts[1],
        );
    }
}

/// The card's blocks: the score with its bar, the four inputs behind it, and who reported the
/// story. Section labels are upper case, so the eye finds a block without reading it.
///
/// `full` adds what only the details view has room for: each source's own headline and the URL
/// `Enter` opens. The card keeps one line per outlet.
fn section_lines(
    view: &View<'_>,
    story: &ScoredStory,
    width: usize,
    full: bool,
) -> Vec<Line<'static>> {
    let t = view.text();
    let label = |text: &str| Line::from(Span::styled(text.to_string(), secondary()));
    let mut lines = vec![label(t.section_score)];

    let (filled, empty) = bar(story.score, 10);
    let score = vec![
        Span::styled(
            format!("{:.2}", story.score),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(filled, Style::default().fg(ACCENT)),
        Span::styled(empty, secondary()),
    ];
    // A movement claim needs the previous ranking to exist. Without one, saying `new` would be a
    // claim about a comparison that never happened.
    if let Some(deltas) = view.deltas {
        let delta = deltas.get(&story.key).copied();
        let (text, style) = match delta {
            Some(movement) if movement > 0 => (
                t.moved(Some(movement), t.window(view.window)),
                Style::default().fg(ACCENT),
            ),
            other => (
                t.moved(other, t.window(view.window)),
                Style::default().fg(NEW),
            ),
        };
        lines.push(Line::from(score));
        lines.push(Line::from(Span::styled(clip(&text, width), style)));
    } else {
        lines.push(Line::from(score));
    }

    lines.push(label(t.section_signals));
    let best = best_velocity(story);
    // The note names the kind of number and how old it is. An estimate and a measurement are
    // different claims, and a count nobody has refreshed for an hour is not today's pace.
    let pace = match best {
        Some(outlet) => match outlet.velocity {
            Some(velocity) => t.pace(velocity.per_hour, outlet.relative_velocity),
            None => t.no_views.to_string(),
        },
        None => t.no_views.to_string(),
    };
    lines.push(signal(
        t.coverage,
        story.coverage_norm,
        &t.outlets(independent_outlets(story)),
        width,
    ));
    lines.push(signal(t.engagement, story.engagement_norm, &pace, width));
    // The quality of the measurement gets its own line. In the note column it would be the
    // first thing clipped, and the kind of number is what the reader has to judge.
    let measured = match best.and_then(|outlet| outlet.velocity) {
        Some(velocity) => t.velocity_note(velocity.basis, (view.now - velocity.observed_at).max(0)),
        None => t.velocity_none.to_string(),
    };
    lines.push(Line::from(Span::styled(
        clip(
            &format!("{}{measured}", " ".repeat(SIGNAL_LABEL + 1)),
            width,
        ),
        dim(),
    )));
    lines.push(signal(
        t.freshness,
        story.freshness,
        &t.ago_sentence((view.now - story.updated_at).max(0)),
        width,
    ));
    lines.push(signal(
        t.spread,
        story.spread_velocity_norm,
        &t.picked_up(story.spread_velocity as i64, view.window.spread_seconds()),
        width,
    ));

    lines.push(label(t.section_sources));
    lines.extend(outlet_lines(view, story, width, full));
    lines
}

/// The post whose pace the engagement row prints: the fastest one any outlet measured, with a
/// measured slope preferred over an estimate, because that is the number the score is built from
/// and the note under it says which kind it is.
pub(crate) fn best_velocity(story: &ScoredStory) -> Option<&OutletContribution> {
    story
        .outlets
        .iter()
        .filter(|outlet| outlet.velocity.is_some())
        .max_by(|a, b| {
            let key = |outlet: &OutletContribution| {
                outlet
                    .velocity
                    .map(|velocity| (velocity.basis == VelocityBasis::Measured, velocity.per_hour))
                    .unwrap_or((false, 0.0))
            };
            key(a).partial_cmp(&key(b)).unwrap_or(Ordering::Equal)
        })
}

/// The best relative pace of the story's posts: the number the growth order sorts on, so the
/// column and the order cannot disagree.
pub(crate) fn best_relative(story: &ScoredStory) -> Option<f64> {
    story
        .outlets
        .iter()
        .filter_map(|outlet| outlet.relative_velocity)
        .max_by(f64::total_cmp)
}

/// One input behind the score: its name, its real normalised value as a bar, and the raw
/// number the normalisation came from. The note is clipped to the card rather than left to the
/// frame: a wrapped line would push the blocks under it, and half a word reads as a bug.
fn signal(label: &str, value: f64, note: &str, width: usize) -> Line<'static> {
    let (filled, empty) = bar(value, SIGNAL_BAR);
    let head = SIGNAL_LABEL + SIGNAL_BAR + 1 + 4 + 2;
    Line::from(vec![
        Span::styled(format!("{label:<SIGNAL_LABEL$}"), secondary()),
        Span::styled(filled, Style::default().fg(ACCENT)),
        Span::styled(empty, secondary()),
        Span::styled(format!(" {value:.2}  "), dim()),
        Span::raw(clip(note, width.saturating_sub(head))),
    ])
}

/// How many outlets carry the story without a citation to anyone else. Repeats and citations
/// carry weight in the index, but they name other people's work, and the note says what is true:
/// no citation found.
pub(crate) fn independent_outlets(story: &ScoredStory) -> i64 {
    story
        .outlets
        .iter()
        .filter(|outlet| outlet.provenance == Provenance::Independent)
        .count() as i64
}

/// Every outlet carrying the story, on two lines each: who and how they carry it, then what
/// their posts measured. One line per outlet does not fit the card's width.
///
/// The outlet `Enter` will open is marked in the margin. Without a marker the key would have to
/// mean "the first one", which is a choice the user did not make and cannot see.
///
/// `full` prints the headline each outlet ran and the address `Enter` opens. Until the details
/// view, the address of the article was nowhere on screen, and the marker pointed at a URL the
/// reader could not read.
fn outlet_lines(
    view: &View<'_>,
    story: &ScoredStory,
    width: usize,
    full: bool,
) -> Vec<Line<'static>> {
    let t = view.text();
    let mut lines = Vec::new();
    for (index, outlet) in story.outlets.iter().enumerate() {
        let chosen = index == view.outlet;
        let marker = if chosen { "▌" } else { " " };
        // The name keeps its line to itself. Provenance labels are long, and a source list whose
        // names are all clipped to `Qafqaz…` cannot be read.
        let tail = format!("  {}", t.ago((view.now - outlet.newest).max(0)));
        lines.push(Line::from(vec![
            Span::styled(
                marker,
                if chosen {
                    Style::default().fg(ACCENT)
                } else {
                    secondary()
                },
            ),
            Span::styled(
                clip(
                    &outlet.outlet,
                    width
                        .saturating_sub(1)
                        .saturating_sub(UnicodeWidthStr::width(tail.as_str())),
                ),
                if chosen { strong() } else { Style::default() },
            ),
            Span::styled(tail, secondary()),
        ]));
        let measured = match (outlet.velocity, outlet.views) {
            (Some(velocity), views) => format!(
                "{} · {} · {} · {}",
                t.provenance(outlet.provenance),
                views.map_or_else(|| t.no_views.to_string(), |views| t.views(views)),
                t.pace(velocity.per_hour, outlet.relative_velocity),
                t.velocity_note(velocity.basis, (view.now - velocity.observed_at).max(0))
            ),
            (None, Some(views)) => {
                format!("{} · {}", t.provenance(outlet.provenance), t.views(views))
            }
            (None, None) => format!("{} · {}", t.provenance(outlet.provenance), t.no_views),
        };
        lines.push(Line::from(Span::styled(
            clip(&format!("   {measured}"), width),
            secondary(),
        )));
        if full {
            // The headline first, then the address. Both are indented under the outlet they
            // belong to, so a reader can tell which source ran which wording.
            for line in wrap(&outlet.title, width.saturating_sub(3), usize::MAX) {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(line, dim()),
                ]));
            }
            lines.push(Line::from(vec![
                Span::raw("   "),
                Span::styled(clip(&outlet.url, width.saturating_sub(3)), dim()),
            ]));
        }
    }
    lines
}

/// The hints as the footer prints them: the key, then what it does.
fn keycaps(keys: &[Keycap]) -> String {
    keys.iter()
        .map(|(key, label)| format!("[{key}] {label}"))
        .collect::<Vec<_>>()
        .join("  ")
}

/// The hints that fit whole. A hint cut mid-word (`l язы`) reads as a broken screen; dropping
/// it sends the reader to the help, which lists every key in full.
fn fitting(hints: &[Keycap], width: u16) -> String {
    let mut out = String::new();
    for (key, label) in hints {
        let entry = format!("[{key}] {label}");
        let candidate = if out.is_empty() {
            entry
        } else {
            format!("{out}  {entry}")
        };
        if UnicodeWidthStr::width(candidate.as_str()) > width as usize {
            break;
        }
        out = candidate;
    }
    if out.is_empty() {
        clip(&keycaps(hints), width as usize)
    } else {
        out
    }
}

fn draw_footer(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let t = view.text();
    // Each mode offers its own keys: in filter mode every printable key is filter text, in the
    // details view the arrows scroll, and offering `q` where it cannot quit would be a lie.
    let (hints, ways_out) = match view.mode {
        Mode::Filter => (t.footer_filter_keys, t.footer_filter_ways_out),
        Mode::Details => (t.footer_details_keys, t.footer_details_ways_out),
        Mode::Help => (t.footer_help_keys, t.footer_help_ways_out),
        Mode::List => (t.footer_keys, t.footer_ways_out),
    };
    let leading = if view.mode == Mode::Filter {
        format!("{}  ", t.filter_label)
    } else {
        String::new()
    };
    // The way out keeps the right edge, so a narrow terminal drops a hint instead of the key
    // that leaves the mode.
    let ways_out_width = UnicodeWidthStr::width(keycaps(ways_out).as_str()) as u16;
    let hints_width = area
        .width
        .saturating_sub(ways_out_width + 2)
        .saturating_sub(UnicodeWidthStr::width(leading.as_str()) as u16);
    let style = dim();

    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("{leading}{}", fitting(hints, hints_width)),
            style,
        )),
        Rect {
            width: area.width.saturating_sub(ways_out_width + 2),
            ..area
        },
    );
    frame.render_widget(
        Paragraph::new(Span::styled(keycaps(ways_out), style)).alignment(Alignment::Right),
        Rect {
            x: area.x + area.width.saturating_sub(ways_out_width),
            width: ways_out_width.min(area.width),
            ..area
        },
    );
}

fn draw_help(frame: &mut Frame, view: &View<'_>, area: Rect) {
    let t = view.text();
    let mut lines: Vec<Line> = t.help_lines.iter().map(|line| Line::from(*line)).collect();
    if !view.degraded.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("{}: {}", t.degraded, view.degraded.join(", ")),
            Style::default().fg(FAILURE),
        )));
    }
    // Sized to the longest hint, so no key is ever cut off mid-word. On a terminal too narrow
    // for that width the text is clipped instead, and every key is still in the list.
    let content = (t
        .help_lines
        .iter()
        .map(|line| line.width())
        .max()
        .unwrap_or(0) as u16)
        .max(UnicodeWidthStr::width(t.help_scroll_hint) as u16);
    let width = (content + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    // The visible height decides how far the overlay may scroll, so the last line can always be
    // reached: the keys below the fold are the ones a short terminal would otherwise hide.
    let visible = height.saturating_sub(3);
    let max_scroll = (lines.len() as u16).saturating_sub(visible);
    let title = if max_scroll == 0 {
        t.help_title.to_string()
    } else {
        format!(
            "{} {}/{} ",
            t.help_title.trim(),
            view.help_scroll.min(max_scroll) + 1,
            max_scroll + 1
        )
    };
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((view.help_scroll.min(max_scroll), 0))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(secondary())
                    .title(title),
            ),
        popup,
    );
    if height > 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(t.help_scroll_hint, dim())),
            Rect {
                x: popup.x + 1,
                y: popup.y + popup.height - 2,
                width: popup.width.saturating_sub(2),
                height: 1,
            },
        );
    }
}

/// The selected story alone, filling the body.
///
/// The card beside the list is capped by the space left over, so a short or narrow terminal cuts
/// its lower blocks off. This view has the whole body and scrolls, which is what keeps every
/// signal and every source reachable on any terminal.
fn draw_details(frame: &mut Frame, view: &View<'_>, area: Rect, selected: usize) {
    let t = view.text();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(secondary())
        .title(t.details_title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = inner.width.saturating_sub(2) as usize;
    if area.width < 4 || area.height < 3 {
        return;
    }
    let Some(story) = view.stories.get(selected) else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                clip(
                    if view.last_poll.is_none() {
                        t.empty_first_run
                    } else {
                        t.empty_window_long
                    },
                    width,
                ),
                dim(),
            )),
            inner,
        );
        return;
    };

    let mut lines: Vec<Line<'static>> = wrap(&story.title, width, usize::MAX)
        .into_iter()
        .map(|line| Line::from(Span::styled(line, strong())))
        .collect();
    lines.push(Line::default());
    lines.extend(content_lines(view, story, width));
    lines.push(Line::default());
    lines.extend(
        section_lines(view, story, width, true)
            .into_iter()
            .map(indent),
    );
    // One column of padding, so no label touches the border.
    let body = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(1),
        ..inner
    };
    frame.render_widget(Paragraph::new(lines).scroll((view.scroll, 0)), body);
}

/// What the story says, in full.
///
/// The marked source's own item is the text, because `Enter` opens that source: the label names
/// it, so the screen never shows one publication's words under another's name. A source that
/// stored no text — a Google News entry is a headline and little else — falls back to the
/// story's lede, and the headline alone is labelled as the story's rather than the source's.
fn content_lines(view: &View<'_>, story: &ScoredStory, width: usize) -> Vec<Line<'static>> {
    let t = view.text();
    let marked = story.outlets.get(view.outlet);
    let stored = marked.and_then(|outlet| view.bodies.get(&outlet.url));
    let (label, text) = match (stored, marked) {
        (Some(body), Some(outlet)) => (
            format!("{}  {}", t.section_content, outlet.outlet),
            Some(body.as_str()),
        ),
        _ => (t.section_content.to_string(), story.description.as_deref()),
    };
    let mut lines = vec![Line::from(Span::styled(clip(&label, width), secondary()))];
    match text {
        Some(text) => lines.extend(
            wrap(text, width, usize::MAX)
                .into_iter()
                .map(|line| Line::from(Span::styled(line, dim()))),
        ),
        None => lines.push(Line::from(Span::styled(clip(t.body_missing, width), dim()))),
    }
    lines
}

/// Shift a line right by one column, for the bordered views.
fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(line.spans);
    Line::from(spans)
}

/// A view count short enough for a column. The exact number is in the card, where there is room
/// to print it, and the column header says what the number is.
pub(crate) fn compact(count: i64) -> String {
    let value = count as f64;
    if value >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.1}K", value / 1_000.0)
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::{OutletContribution, Provenance, ScoredStory};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::LazyLock;

    /// The map a `View` gets when no details view asked the store for anything yet.
    static NO_BODIES: LazyLock<HashMap<String, String>> = LazyLock::new(HashMap::new);

    fn story(title: &str, coverage: f64) -> ScoredStory {
        ScoredStory {
            key: title.to_string(),
            title: title.to_string(),
            description: None,
            score: 0.9,
            coverage,
            coverage_norm: 1.0,
            engagement: 1.2,
            engagement_norm: 0.8,
            freshness: 0.7,
            engagement_basis: Some(VelocityBasis::Measured),
            engagement_observed_at: Some(0),
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
                provenance: Provenance::Independent,
                newest: 0,
                views: Some(2650),
                velocity: Some(crate::score::Velocity {
                    per_hour: 900.0,
                    basis: VelocityBasis::Measured,
                    observed_seconds: 3600,
                    observed_at: 0,
                }),
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

    fn render_at(view: &View<'_>, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| draw(frame, view)).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(visible)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render(view: &View<'_>) -> String {
        render_at(view, 100, 30)
    }

    fn base<'a>(
        stories: &'a [ScoredStory],
        deltas: Option<&'a std::collections::HashMap<String, i64>>,
    ) -> View<'a> {
        View {
            lang: Lang::En,
            window: Window::Day,
            stories,
            bodies: &NO_BODIES,
            window_total: stories.len(),
            selected: 0,
            outlet: 0,
            deltas,
            comparison_at: deltas.map(|_| 0),
            filter: "",
            mode: Mode::List,
            sort: Sort::Index,
            scroll: 0,
            help_scroll: 0,
            sources_ok: Some(12),
            sources_total: 20,
            last_poll: Some(0),
            degraded: &[],
            now: 0,
            status: "",
        }
    }

    #[test]
    fn header_reports_source_health_and_the_active_window() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let screen = render(&base(&stories, None));
        assert!(screen.contains("bakutrend"), "{screen}");
        assert!(screen.contains("12/20 sources ok"), "{screen}");
        assert!(screen.contains("[ 24h ]"), "{screen}");
        assert!(screen.contains("Bakıda bu yollar bağlıdır"), "{screen}");
        assert!(screen.contains("1 story"), "{screen}");
    }

    #[test]
    fn a_header_with_no_poll_yet_says_so_instead_of_inventing_health() {
        let stories = vec![story("Bir xəbər", 1.0)];
        let mut view = base(&stories, None);
        view.sources_ok = None;
        view.last_poll = None;
        let screen = render(&view);
        assert!(screen.contains("sources unknown"), "{screen}");
        assert!(screen.contains("polled never"), "{screen}");

        // An empty screen with no poll yet is the first log line's news, not a broken window.
        let empty: Vec<ScoredStory> = Vec::new();
        let mut view = base(&empty, None);
        view.sources_ok = None;
        view.last_poll = None;
        let screen = render(&view);
        assert!(screen.contains("the first poll is"), "{screen}");
        assert!(!screen.contains("No stories in this window"), "{screen}");
    }

    #[test]
    fn the_movement_column_is_absent_until_comparable_history_exists() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let without = render(&base(&stories, None));
        assert!(
            !without.contains("NEW"),
            "no movement column without history:\n{without}"
        );
        assert!(
            !without.contains("since the previous"),
            "and no movement claim either:\n{without}"
        );

        let deltas =
            std::collections::HashMap::from([("Bakıda bu yollar bağlıdır".to_string(), 3i64)]);
        let with = render(&base(&stories, Some(&deltas)));
        assert!(
            with.contains("+3"),
            "a rising story shows its movement:\n{with}"
        );
        assert!(
            with.contains("+3 since the previous 24h window"),
            "and the card spells it out:\n{with}"
        );

        // A story that was not in the previous ranking says so, in the column and in the card.
        let fresh = render(&base(&stories, Some(&std::collections::HashMap::new())));
        assert!(fresh.contains("NEW"), "{fresh}");
        assert!(
            fresh.contains("new since the previous 24h window"),
            "{fresh}"
        );
    }

    /// The list has to show which row the card belongs to without relying on colour.
    #[test]
    fn the_selected_row_is_marked_and_the_card_follows_it() {
        let mut second = story("İkinci xəbər", 5.0);
        second.description = Some("İkinci xəbərin təsviri budur.".into());
        let stories = vec![story("Birinci xəbər", 1.0), second];

        let mut view = base(&stories, None);
        view.selected = 1;
        let screen = render(&view);
        assert!(
            screen.contains("▌ 02"),
            "the marker sits on the selected row:\n{screen}"
        );
        assert!(
            screen.contains("İkinci xəbərin təsviri budur."),
            "the card must show the story the list highlights:\n{screen}"
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
    fn a_stale_selection_does_not_split_the_list_and_the_card() {
        let mut second = story("İkinci xəbər", 5.0);
        second.description = Some("İkinci xəbərin təsviri budur.".into());
        let stories = vec![story("Birinci xəbər", 1.0), second];
        let mut view = base(&stories, None);
        view.selected = 99;
        let screen = render(&view);
        assert!(
            screen.contains("İkinci xəbərin təsviri budur."),
            "the card must show the story the list highlights:\n{screen}"
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
        // `last_poll: Some(0)` means a poll HAS run: the card must not claim one is still
        // running (I5). The status keeps its own row.
        assert!(screen.contains("No stories in this window"), "{screen}");
        assert!(
            screen.contains("database error: disk I/O error"),
            "a failure must not hide behind a cheerful empty state:\n{screen}"
        );

        // A filter that matches nothing says so, and says how many stories it looked through.
        view.filter = "külək";
        view.window_total = 14;
        let screen = render(&view);
        assert!(screen.contains("nothing matches"), "{screen}");
        assert!(screen.contains("14 in 24h"), "{screen}");
    }

    /// A failure cut off by a small terminal is the one line the user cannot do without, so it
    /// gets its own row rather than the card's tail.
    #[test]
    fn the_status_survives_a_terminal_too_small_for_the_card() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let mut view = base(&stories, None);
        view.status = "database read failed: disk I/O error";
        let screen = render_at(&view, 60, 14);
        assert!(screen.contains("database read failed"), "{screen}");
    }

    /// A thin window names its own count and offers the wider period. It never widens it.
    #[test]
    fn a_thin_window_offers_the_wider_period_without_taking_it() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let mut view = base(&stories, None);
        view.window_total = 1;
        let screen = render(&view);
        assert!(screen.contains("1 story in the last 24 hours"), "{screen}");
        assert!(screen.contains("press w for the last week"), "{screen}");
        assert!(
            screen.contains("[ 24h ]"),
            "the tab still names the window it is showing"
        );
    }

    #[test]
    fn the_help_overlay_is_hidden_until_asked_for() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let mut view = base(&stories, None);
        assert!(!render(&view).contains("switch window"));

        view.mode = Mode::Help;
        let screen = render(&view);
        assert!(screen.contains("switch window"), "{screen}");
        assert!(screen.contains("poll now"), "{screen}");
    }

    /// The keys below the fold are the ones a short terminal needs, so the overlay scrolls and
    /// says that it does.
    #[test]
    fn the_help_overlay_scrolls_instead_of_clipping_its_lower_keys() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let mut view = base(&stories, None);
        view.mode = Mode::Help;

        // Twelve rows cannot hold the whole list, and the last key is below the fold.
        let top = render_at(&view, 50, 12);
        assert!(top.contains("switch window"), "{top}");
        assert!(
            !top.contains("[q] quit"),
            "the last line is cut off:\n{top}"
        );
        assert!(
            top.contains("j/k scroll"),
            "the overlay says how to reach it:\n{top}"
        );
        assert!(top.contains("1/"), "and where the reader is:\n{top}");

        view.help_scroll = 200;
        let bottom = render_at(&view, 50, 12);
        assert!(
            bottom.contains("quit"),
            "the end of the list is reachable:\n{bottom}"
        );
        assert!(
            bottom
                .lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 50),
            "{bottom}"
        );

        // And the full list is readable on a terminal that can hold it.
        view.help_scroll = 0;
        let wide = render_at(&view, 100, 30);
        assert!(wide.contains("switch window"), "{wide}");
        assert!(wide.contains("quit"), "{wide}");
    }

    #[test]
    fn the_card_shows_the_index_breakdown_and_every_outlet() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let screen = render(&base(&stories, None));
        assert!(screen.contains("PROMINENCE INDEX"), "{screen}");
        assert!(screen.contains("SIGNALS"), "{screen}");
        assert!(screen.contains("SOURCES"), "{screen}");
        assert!(screen.contains("Coverage"), "{screen}");
        assert!(screen.contains("Engagement"), "{screen}");
        assert!(screen.contains("Freshness"), "{screen}");
        assert!(screen.contains("Spread"), "{screen}");
        assert!(screen.contains("Qafqazinfo"), "{screen}");
        // A reader must not read the absence of a citation as proof of original reporting.
        assert!(screen.contains("no citation seen"), "{screen}");
        // And the pace behind the engagement number says how it was obtained.
        assert!(screen.contains("measured"), "{screen}");
        assert!(!screen.contains("independent"), "{screen}");
    }

    #[test]
    fn the_card_shows_the_storys_short_description() {
        let mut first = story("Bakıda bu yollar bağlıdır", 3.0);
        first.description =
            Some("Şəhər İcra Hakimiyyəti yolların təmir qrafikini açıqladı.".into());
        let stories = vec![first, story("İkinci xəbər", 1.0)];
        let screen = render(&base(&stories, None));
        // The card wraps its text, so only a word is guaranteed to sit on one row.
        assert!(
            screen.contains("qrafikini"),
            "a reader decides from the description, not the headline alone:\n{screen}"
        );

        // A story whose sources sent no body must not print the word `None`.
        let mut view = base(&stories, None);
        view.selected = 1;
        let screen = render(&view);
        assert!(!screen.contains("None"), "{screen}");
        assert!(
            !screen.contains("qrafikini"),
            "the card follows the selection:\n{screen}"
        );
    }

    #[test]
    fn the_card_sits_beside_the_list_when_wide_and_under_it_when_narrow() {
        let mut first = story("Bakıda bu yollar bağlıdır", 3.0);
        first.description = Some("Yolların təmiri sentyabrın sonuna qədər davam edəcək.".into());
        let stories = vec![first];
        let view = base(&stories, None);

        // Both layouts carry the whole screen: header, list, card, footer. A narrow terminal
        // loses pixels, not features.
        for (width, height) in [(120u16, 30u16), (80, 40)] {
            let screen = render_at(&view, width, height);
            assert!(screen.contains("12/20 sources ok"), "{width}:\n{screen}");
            assert!(
                screen.contains("Bakıda bu yollar bağlıdır"),
                "{width}:\n{screen}"
            );
            assert!(screen.contains("Coverage"), "{width}:\n{screen}");
            assert!(screen.contains("Qafqazinfo"), "{width}:\n{screen}");
            assert!(screen.contains("[q] quit"), "{width}:\n{screen}");
            assert!(
                screen
                    .lines()
                    .all(|line| unicode_width::UnicodeWidthStr::width(line) <= width as usize),
                "{width}:\n{screen}"
            );
        }
    }

    /// A stacked card that kept its full height would leave a twenty-row terminal with no
    /// stories on it. The list keeps most of the height; the card's lower blocks are what else
    /// fits.
    #[test]
    fn a_short_terminal_still_ranks_stories() {
        let mut first = story("Bakıda bu yollar bağlıdır", 3.0);
        first.description = Some("Yolların təmiri sentyabrın sonuna qədər davam edəcək.".into());
        let stories = vec![
            first,
            story("İkinci xəbər", 1.0),
            story("Üçüncü xəbər", 1.0),
            story("Dördüncü xəbər", 1.0),
            story("Beşinci xəbər", 1.0),
        ];
        let screen = render_at(&base(&stories, None), 80, 24);
        assert!(
            screen.contains("Beşinci xəbər"),
            "the list keeps its rows:\n{screen}"
        );
        assert!(
            screen.contains("Bakıda bu yollar bağlıdır"),
            "the card keeps the headline:\n{screen}"
        );
        assert!(screen.contains("PROMINENCE INDEX"), "{screen}");
        assert!(screen.contains("[q] quit"), "{screen}");
    }

    #[test]
    fn the_screen_speaks_the_language_it_was_given() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];

        let mut azerbaijani = base(&stories, None);
        azerbaijani.lang = Lang::Az;
        let screen = render(&azerbaijani);
        assert!(screen.contains("mənbə işləyir"), "{screen}");
        assert!(screen.contains("BAŞLIQ"), "{screen}");
        assert!(screen.contains("[?] kömək"), "{screen}");
        assert!(screen.contains("[q] çıx"), "{screen}");

        let mut russian = base(&stories, None);
        russian.lang = Lang::Ru;
        let screen = render(&russian);
        assert!(screen.contains("источников отвечают"), "{screen}");
        assert!(screen.contains("ЗАГОЛОВОК"), "{screen}");
        assert!(screen.contains("[?] помощь"), "{screen}");
        assert!(screen.contains("[q] выход"), "{screen}");
        // The news is not translated: only the chrome around it.
        assert!(screen.contains("Bakıda bu yollar bağlıdır"), "{screen}");
    }

    /// The details view is the only place with room for the whole text, and it shows the text of
    /// the source the marker is on: `Enter` opens that source, so the label names it and the
    /// screen never sets one publication's words under another's name. A source that stored
    /// nothing falls back to the story's lede rather than to an empty block.
    #[test]
    fn the_details_view_shows_the_text_of_the_source_the_marker_is_on() {
        let mut selected = story("Bakıda yollar bağlıdır", 3.0);
        selected.description = Some("Storinin özü üçün qısa giriş.".into());
        let second = OutletContribution {
            outlet: "Report.az".into(),
            title: "Bakıda yollar bağlıdır — Report.az".into(),
            url: "https://report.az/news/1".into(),
            ..selected.outlets[0].clone()
        };
        selected.outlets.push(second);
        let stories = vec![selected];

        let bodies = HashMap::from([(
            "https://report.az/news/1".to_string(),
            "Report.az-ın tam mətni: yolun təmiri sentyabrın sonuna qədər davam edəcək."
                .to_string(),
        )]);
        let mut view = base(&stories, None);
        view.mode = Mode::Details;
        view.bodies = &bodies;
        view.outlet = 1;

        let screen = render_at(&view, 100, 40);
        assert!(screen.contains("CONTENT  Report.az"), "{screen}");
        assert!(screen.contains("Report.az-ın tam mətni"), "{screen}");
        assert!(
            screen.contains("https://report.az/news/1"),
            "the address Enter opens is on screen:\n{screen}"
        );
        assert!(
            screen.contains("Bakıda yollar bağlıdır — Report.az"),
            "each source's own wording is on screen:\n{screen}"
        );
        assert!(
            !screen.contains("Storinin özü"),
            "the marked source's text, not the story lede:\n{screen}"
        );

        // The first source stored no text: the story's own lede is what the reader gets, and the
        // label names no outlet, because the words are not that outlet's.
        view.outlet = 0;
        let screen = render_at(&view, 100, 40);
        assert!(!screen.contains("CONTENT  Report.az"), "{screen}");
        assert!(screen.contains("Storinin özü üçün qısa giriş."), "{screen}");

        // Nothing stored anywhere: say that, and do not print a blank block as if it were text.
        let mut bare = story("Uzun xəbər", 1.0);
        bare.outlets[0].url = "https://qafqazinfo.az/nothing".into();
        let bare_stories = vec![bare];
        let mut view = base(&bare_stories, None);
        view.mode = Mode::Details;
        let screen = render_at(&view, 100, 40);
        assert!(
            screen.contains("no text stored for this publication"),
            "{screen}"
        );
    }

    #[test]
    fn the_footer_offers_the_keys_of_the_mode_the_user_is_in() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let screen = render(&base(&stories, None));
        assert!(screen.contains("[Enter] open"), "{screen}");
        // `/` has to be visible: the footer is the only hint on screen, and the filter mode
        // it opens has no other way in.
        assert!(screen.contains("[/] filter"), "{screen}");
        assert!(screen.contains("[l] language"), "{screen}");
        assert!(screen.contains("[?] help"), "{screen}");

        let mut typing = base(&stories, None);
        typing.mode = Mode::Filter;
        let screen = render(&typing);
        assert!(screen.contains("filter"), "{screen}");
        assert!(screen.contains("[Backspace] deletes"), "{screen}");
        assert!(screen.contains("[Enter] keep"), "{screen}");
        assert!(screen.contains("[Esc] clear"), "{screen}");
        // Every printable key is filter text, so `q` cannot quit here.
        assert!(!screen.contains("[q] quit"), "{screen}");

        // The details view and the help offer their own keys, and the way out of each one.
        typing.mode = Mode::Details;
        let screen = render(&typing);
        assert!(screen.contains("[j/k] scroll"), "{screen}");
        assert!(screen.contains("[Esc] back"), "{screen}");

        typing.mode = Mode::Help;
        let screen = render(&typing);
        assert!(screen.contains("[j/k] scroll"), "{screen}");
        assert!(screen.contains("[Esc] close"), "{screen}");
    }

    #[test]
    fn the_ways_out_survive_a_terminal_too_narrow_for_the_hints() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        let screen = render_at(&base(&stories, None), 40, 24);
        assert!(screen.contains("[q] quit"), "{screen}");
    }

    /// The switcher is discoverable from the screen itself: no help, no config file, no restart.
    #[test]
    fn the_footer_names_the_language_key_in_every_language() {
        let stories = vec![story("Bakıda bu yollar bağlıdır", 3.0)];
        for lang in Lang::ALL {
            let mut view = base(&stories, None);
            view.lang = lang;
            let screen = render_at(&view, 120, 30);
            assert!(
                screen.contains(view.text().language),
                "{lang:?}: the footer must name the switcher:\n{screen}"
            );
        }
    }

    /// A hint cut mid-word is worse than a hint that is not there: the help still lists every
    /// key in full, and nothing on screen lies about what it says.
    #[test]
    fn a_hint_that_does_not_fit_is_dropped_whole() {
        let keys: &[Keycap] = &[("a", "one"), ("b", "two"), ("c", "three")];
        assert_eq!(fitting(keys, 60), "[a] one  [b] two  [c] three");
        assert_eq!(fitting(keys, 22), "[a] one  [b] two");
        assert_eq!(fitting(keys, 10), "[a] one");
        assert_eq!(fitting(keys, 4), "[a]…", "a lone hint too wide is cut");
        assert_eq!(fitting(keys, 0), "…");
    }

    /// The sizes the work has to hold at: a full 80x24, and two terminals small enough that the
    /// card cannot show everything beside the list. Nothing may overflow the frame, and every
    /// source must still be reachable, which is what the details view is for.
    #[test]
    fn the_screen_holds_at_eighty_by_twenty_four_and_smaller() {
        let mut first = story("Bakıda bu yollar bağlıdır", 3.0);
        first.description = Some("Yolların təmiri sentyabrın sonuna qədər davam edəcək.".into());
        let template = first.outlets[0].clone();
        first.outlets = (0..6)
            .map(|index| OutletContribution {
                outlet: format!("Outlet{index}"),
                ..template.clone()
            })
            .collect();
        let stories = vec![
            first,
            story("İkinci xəbər", 1.0),
            story("Üçüncü xəbər", 1.0),
            story("Dördüncü xəbər", 1.0),
        ];

        for (width, height) in [(80u16, 24u16), (60, 18), (50, 12)] {
            let list = render_at(&base(&stories, None), width, height);
            assert!(
                list.lines()
                    .all(|line| unicode_width::UnicodeWidthStr::width(line) <= width as usize),
                "{width}x{height}:\n{list}"
            );
            assert!(list.contains("bakutrend"), "{width}x{height}:\n{list}");
            assert!(list.contains("[q] quit"), "{width}x{height}:\n{list}");

            // Scrolling the details view reaches every source on every one of those sizes.
            let mut details = base(&stories, None);
            details.mode = Mode::Details;
            let mut seen = String::new();
            for scroll in 0..40u16 {
                details.scroll = scroll;
                let screen = render_at(&details, width, height);
                assert!(
                    screen
                        .lines()
                        .all(|line| unicode_width::UnicodeWidthStr::width(line) <= width as usize),
                    "{width}x{height} at scroll {scroll}:\n{screen}"
                );
                seen.push_str(&screen);
            }
            for index in 0..6 {
                assert!(
                    seen.contains(&format!("Outlet{index}")),
                    "{width}x{height}: source {index} is unreachable by scrolling"
                );
            }

            // And so is every line of the help.
            let mut help = base(&stories, None);
            help.mode = Mode::Help;
            let mut reachable = String::new();
            for scroll in 0..40u16 {
                help.help_scroll = scroll;
                reachable.push_str(&render_at(&help, width, height));
            }
            let t = Lang::En.strings();
            for line in t.help_lines {
                let key = line.split_whitespace().next().unwrap_or_default();
                assert!(
                    reachable.contains(key),
                    "{width}x{height}: the help line for {key} is unreachable"
                );
            }
        }
    }

    /// No screen may show a template placeholder. Every one is filled from the strings table,
    /// and an unfilled `{ago}` on screen is a sentence the program never finished.
    #[test]
    fn no_screen_shows_an_unfilled_placeholder() {
        let stories = vec![
            story("Bakıda bu yollar bağlıdır", 3.0),
            story("İkinci xəbər", 1.0),
        ];
        for lang in Lang::ALL {
            for (mode, sort) in [
                (Mode::List, Sort::Index),
                (Mode::List, Sort::Growth),
                (Mode::List, Sort::Views),
                (Mode::Details, Sort::Index),
                (Mode::Help, Sort::Index),
                (Mode::Filter, Sort::Index),
            ] {
                let mut view = base(&stories, None);
                view.lang = lang;
                view.mode = mode;
                view.sort = sort;
                view.filter = "xəbər";
                view.status = "poll failed: connection reset";
                let screen = render_at(&view, 100, 30);
                assert!(
                    !screen.contains('{'),
                    "{lang:?} {mode:?} {sort:?} left a placeholder on screen:\n{screen}"
                );
            }
        }
        // A story with no view sample at all takes the other branch of the note.
        let mut bare = story("Ölçməsi olmayan xəbər", 1.0);
        bare.outlets[0].velocity = None;
        bare.outlets[0].views = None;
        let stories = vec![bare];
        let screen = render(&base(&stories, None));
        assert!(!screen.contains('{'), "{screen}");
    }

    /// The selected row is marked by shape as well as by style, so a terminal whose palette
    /// hides the accent still shows which row is selected, and the marker column is one the
    /// terminal's own colours cannot remove.
    #[test]
    fn the_selection_is_visible_without_colour() {
        let stories = vec![
            story("Bakıda bu yollar bağlıdır", 3.0),
            story("İkinci xəbər", 1.0),
        ];
        let mut view = base(&stories, None);
        view.selected = 1;
        let screen = render_at(&view, 120, 40);
        assert_eq!(
            screen.matches('▌').count(),
            2,
            "one marker for the list row and one for the chosen source:\n{screen}"
        );
        let marked = screen
            .lines()
            .find(|line| line.contains('▌') && line.contains("İkinci xəbər"))
            .unwrap_or_else(|| panic!("the selected list row carries the marker:\n{screen}"));
        assert!(
            marked.contains("02"),
            "the marked row is the second one:\n{screen}"
        );
    }

    #[test]
    fn a_very_long_headline_does_not_panic_or_overflow() {
        // Double-width characters and a unique tail: the ellipsis proves the list clipped it
        // rather than letting the row run past the frame.
        let long = format!("{}SON", "Bakıda gözəl 世界 xəbər ".repeat(12));
        let stories = vec![story(&long, 1.0)];
        let screen = render(&base(&stories, None));
        assert!(
            screen.contains("Bakıda gözəl 世界"),
            "the headline must still render, wide characters included:\n{screen}"
        );
        assert!(
            screen.contains('…'),
            "a headline longer than its column is cut, not left to spill:\n{screen}"
        );
        assert!(
            screen
                .lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 100),
            "{screen}"
        );
    }

    /// The card's line count has to be exact: the sources block sits under a fixed box, and a
    /// description nobody counted would push it out of the frame.
    #[test]
    fn a_long_description_does_not_push_the_sources_out_of_the_card() {
        let mut first = story("Bakıda bu yollar bağlıdır", 3.0);
        first.description = Some("Şəhər İcra Hakimiyyəti ".repeat(30));
        first.outlets[0].outlet = "Azərbaycan Milli Xəbər Agentliyi və Ortadoğu Bürosu".repeat(2);
        let stories = vec![first];
        let screen = render_at(&base(&stories, None), 80, 40);
        assert!(screen.contains("SOURCES"), "{screen}");
        assert!(
            screen.contains("Qafqazinfo") || screen.contains("Azərbaycan"),
            "{screen}"
        );
        assert!(
            screen
                .lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 80),
            "{screen}"
        );
        assert!(
            screen.contains('…'),
            "a long outlet is cut, not spilled:\n{screen}"
        );
    }
}
