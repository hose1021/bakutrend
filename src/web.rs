//! HTTP surface: the same ranking the TUI draws, as one page and as JSON.
//!
//! Read-only by construction: every request opens `Store::open_read_only`, so a server cannot
//! race the poller's writer on the WAL and cannot damage history. `rank_window` is the one
//! ranking implementation, `Strings` the one set of words and `ui` the one place the card's facts
//! are read, so the page and the TUI cannot disagree about a score, a notice or a sentence.
//!
//! The page carries no script: the window, the order, the language and the selection are links
//! and one form, so it works from a keyboard, from a screen reader and with scripting off.
//!
//! One renderer serves two hosts. A server answers a query, so its links carry the view as
//! parameters; a static snapshot has nothing to answer one, so each view is a file of its own and
//! a link names that file ([`Links`]). `export` writes such a snapshot.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;

use crate::app::{App, Sort};
use crate::config::Config;
use crate::i18n::{Lang, Strings};
use crate::poller::PollReport;
use crate::score::{OutletContribution, ScoredStory};
use crate::store::{Store, Window};
use crate::ui::{Level, View, best_relative, best_velocity, compact, independent_outlets, notice};

/// What a request asks the page for. Every one of them is a link the page itself renders, so a
/// reader can reach all of them without typing a URL.
#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    window: Option<String>,
    sort: Option<String>,
    filter: Option<String>,
    lang: Option<String>,
    /// The story the card explains, counted from one as the list ranks it.
    story: Option<usize>,
}

/// The last poll report, shared with the poller thread that runs beside the server. The page's
/// health line comes from the same report the TUI's poll event carries, so neither front end can
/// claim a source count the poller did not observe.
pub type LastPoll = Arc<Mutex<Option<PollReport>>>;

/// What a request needs that no query carries, held for the life of the server. The store is
/// opened per request rather than shared: a reader sees the poll that just landed.
#[derive(Clone)]
pub struct WebState {
    db_path: PathBuf,
    config: Config,
    last_poll: LastPoll,
}

fn parse_window(value: Option<&str>) -> Result<Window, String> {
    match value.unwrap_or("24h") {
        "1h" => Ok(Window::Hour),
        "24h" | "day" => Ok(Window::Day),
        "7d" | "week" => Ok(Window::Week),
        other => Err(format!("unknown window `{other}`: use 1h, 24h or 7d")),
    }
}

/// The order the list is shown in. The words a reader sees are `Strings::sort`; these are the
/// ASCII names a URL carries.
fn parse_sort(value: Option<&str>) -> Result<Sort, String> {
    match value.unwrap_or("index") {
        "index" => Ok(Sort::Index),
        "growth" => Ok(Sort::Growth),
        "coverage" => Ok(Sort::Coverage),
        "views" => Ok(Sort::Views),
        "chronology" => Ok(Sort::Chronology),
        other => Err(format!(
            "unknown sort `{other}`: use index, growth, coverage, views or chronology"
        )),
    }
}

fn sort_slug(sort: Sort) -> &'static str {
    match sort {
        Sort::Index => "index",
        Sort::Growth => "growth",
        Sort::Coverage => "coverage",
        Sort::Views => "views",
        Sort::Chronology => "chronology",
    }
}

/// The model for one request: a read-only ranking of the window it asked for, ordered, narrowed
/// and selected as it asked. One `App`, so the page reads the same numbers the TUI does.
#[allow(clippy::result_large_err)] // the Err carries a whole `Response`, as the handlers' do.
fn model(state: &WebState, query: &PageQuery) -> Result<App, Response> {
    let window = parse_window(query.window.as_deref()).map_err(bad_request)?;
    let sort = parse_sort(query.sort.as_deref()).map_err(bad_request)?;
    let store = Store::open_read_only(&state.db_path).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("open {}: {error}", state.db_path.display()),
        )
            .into_response()
    })?;
    let now = chrono::Utc::now().timestamp();
    let mut app = App::new(store, state.config.clone(), now);
    if let Some(lang) = query.lang.as_deref().and_then(Lang::parse) {
        app.lang = lang;
    }
    // The health line is the poller's own report. Until one cycle has finished there is no
    // report, and the page says `sources unknown` rather than inventing a count.
    let report = state
        .last_poll
        .lock()
        .expect("poll report mutex poisoned")
        .clone();
    if let Some(report) = &report {
        app.record_poll(report, now);
    }
    app.set_view(
        window,
        sort,
        query.filter.clone().unwrap_or_default(),
        query.story.unwrap_or(1).saturating_sub(1),
    );
    Ok(app)
}

fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

#[allow(clippy::result_large_err)] // the Err carries a whole `Response`; boxing it would
// satisfy the lint by hiding the size the handler genuinely returns.
async fn index(
    State(state): State<Arc<WebState>>,
    Query(query): Query<PageQuery>,
) -> Result<Html<String>, Response> {
    let mut app = model(&state, &query)?;
    // The card prints the story's own text where the store has it, which is the one read the
    // list never needs.
    app.load_bodies();
    Ok(Html(page(&app.view(), Links::Query)))
}

/// The same ranking as JSON, for a reader who wants the numbers rather than the page.
#[allow(clippy::result_large_err)] // as above: the error is a `Response`.
async fn stories(
    State(state): State<Arc<WebState>>,
    Query(query): Query<PageQuery>,
) -> Result<axum::Json<serde_json::Value>, Response> {
    let app = model(&state, &query)?;
    Ok(axum::Json(stories_json(&app.view())))
}

/// The numbers behind one view, as the JSON endpoint returns them and as a snapshot writes them.
pub fn stories_json(view: &View<'_>) -> serde_json::Value {
    serde_json::json!({
        "window": view.window.label(),
        "sort": sort_slug(view.sort),
        "computed_at": view.now,
        "stories": view.stories,
    })
}

pub async fn serve(
    db_path: &Path,
    config: Config,
    bind: &str,
    last_poll: LastPoll,
) -> Result<(), String> {
    // Opened once and dropped: a database that cannot be read is a startup failure the user has
    // to see, not a server that answers every request with the same error.
    Store::open_read_only(db_path)
        .map_err(|error| format!("open {}: {error}", db_path.display()))?;
    let addr: SocketAddr = bind
        .parse()
        .map_err(|_| format!("invalid bind address `{bind}`: expected host:port"))?;
    let state = WebState {
        db_path: db_path.to_path_buf(),
        config,
        last_poll,
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/stories", get(stories))
        .with_state(Arc::new(state));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| format!("bind {addr}: {error}"))?;
    println!("bakutrend web UI on http://{addr}");
    axum::serve(listener, app)
        .await
        .map_err(|error| error.to_string())
}

/// Where the page's links point.
///
/// A server answers a query, so its links carry the view as parameters. A static snapshot has
/// nothing to answer one: each view is a file of its own, and a link names that file. One
/// renderer for both, so the page cannot drift between the two hosts.
#[derive(Clone, Copy)]
pub enum Links<'a> {
    /// `/?window=24h&sort=index&lang=en`.
    Query,
    /// A snapshot. `prefix` climbs from the page being rendered back to the site root — `""` for
    /// the root page, `"../../../"` for a page three directories deep. `built` is the moment the
    /// ranking was read: the page states it, because every age on it is measured from that moment
    /// and not from the reader's own clock.
    Snapshot { prefix: &'a str, built: &'a str },
}

/// The file that holds one view of a snapshot, relative to its root. The default view is the
/// site's front door; every other view is a directory of its own.
///
/// `export` writes these files and the page links to them through this one function, so a link
/// cannot name a file that was never written.
pub fn page_path(lang: Lang, window: Window, sort: Sort) -> String {
    if (lang, window, sort) == (Lang::En, Window::Day, Sort::Index) {
        return "index.html".to_string();
    }
    format!(
        "{}/{}/{}/index.html",
        lang.code(),
        window.label(),
        sort_slug(sort)
    )
}

/// The file that holds one view's numbers, relative to a snapshot's root. The numbers carry no
/// words, so the language is not part of the path.
pub fn stories_path(window: Window, sort: Sort) -> String {
    format!("api/{}/{}.json", window.label(), sort_slug(sort))
}

/// How far a page has to climb to reach the site root: one `../` for every directory in its own
/// path. Derived from the path rather than counted by hand, so moving a page cannot leave it
/// linking one level short.
pub fn back_to_root(file: &str) -> String {
    "../".repeat(file.matches('/').count())
}

/// A link back into this page. Every control carries the whole view, so following one never
/// silently drops the window, the order, the filter or the language.
struct Link<'a> {
    links: Links<'a>,
    window: Window,
    sort: Sort,
    lang: Lang,
    filter: &'a str,
    story: Option<usize>,
}

impl<'a> Link<'a> {
    fn of(view: &View<'a>, links: Links<'a>) -> Self {
        Self {
            links,
            window: view.window,
            sort: view.sort,
            lang: view.lang,
            filter: view.filter,
            story: None,
        }
    }

    fn window(mut self, window: Window) -> Self {
        self.window = window;
        self
    }

    fn sort(mut self, sort: Sort) -> Self {
        self.sort = sort;
        self
    }

    fn lang(mut self, lang: Lang) -> Self {
        self.lang = lang;
        self
    }

    fn story(mut self, story: usize) -> Self {
        self.story = Some(story);
        self
    }

    /// The page this link names. Every window, order and language has one, on a server and in a
    /// snapshot alike.
    fn path(&self) -> String {
        match self.links {
            Links::Query => {
                let mut path = format!(
                    "/?window={}&sort={}&lang={}",
                    self.window.label(),
                    sort_slug(self.sort),
                    self.lang.code()
                );
                if !self.filter.is_empty() {
                    path.push_str("&filter=");
                    path.push_str(&encode(self.filter));
                }
                path
            }
            Links::Snapshot { prefix, .. } => {
                format!("{prefix}{}", page_path(self.lang, self.window, self.sort))
            }
        }
    }

    /// The card that explains one story, or `None` where no page holds it.
    ///
    /// A snapshot has no such page. A card is addressed by rank, and the week window ranks over
    /// a thousand stories, so one file per rank is not a site that can be written; on a server
    /// the card is part of the page and the rank is a parameter of it.
    fn card_path(&self) -> Option<String> {
        let story = self.story?;
        match self.links {
            // The card is where the story is explained. A reader on a narrow screen sees it only
            // after the whole list, so choosing a row brings the card into view instead of
            // leaving them at the top of the ranking with nothing to show for the click.
            Links::Query => Some(format!("{}&story={}#card", self.path(), story + 1)),
            Links::Snapshot { .. } => None,
        }
    }
}

/// The page for one request. Pure: the same `View` always renders the same bytes, and the tests
/// in this module read it.
pub fn page(view: &View<'_>, links: Links<'_>) -> String {
    let t = view.lang.strings();
    let selected = view.selected.min(view.stories.len().saturating_sub(1));
    let mut html = String::with_capacity(8192);
    html.push_str("<!doctype html>\n<html lang=\"");
    html.push_str(view.lang.code());
    html.push_str("\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    html.push_str("<meta name=\"color-scheme\" content=\"light dark\">\n");
    html.push_str("<meta name=\"referrer\" content=\"no-referrer\">\n");
    // The browser's own chrome takes the page's ground, so a phone's address bar does not sit in
    // the wrong theme over a page that is not.
    html.push_str(
        "<meta name=\"theme-color\" content=\"#f6f9fb\" media=\"(prefers-color-scheme: light)\">\n",
    );
    html.push_str(
        "<meta name=\"theme-color\" content=\"#0d1115\" media=\"(prefers-color-scheme: dark)\">\n",
    );
    // An empty data URL: the program ships no icon, and without this every visit logs a 404 for
    // one it never had.
    html.push_str("<link rel=\"icon\" href=\"data:,\">\n");
    html.push_str("<title>bakutrend</title>\n<style>\n");
    html.push_str(CSS);
    html.push_str("</style>\n</head>\n<body>\n");

    // How much of the data behind the tabs this program actually has, next to which window the
    // tabs claim, so the two are read together. The names of the sources that are failing are
    // not repeated here: the notice below carries them, from the same poll report.
    let polled = match view.last_poll {
        Some(ts) => t.polled((view.now - ts).max(0)),
        None => format!("{} {}", t.polled, t.never),
    };
    // The dot beside the health line takes its state from the numbers the words beside it carry.
    // It shows the state at a glance and never states one on its own.
    let health = match view.sources_ok {
        None => "off",
        Some(ok) if ok == view.sources_total => "ok",
        Some(_) => "warn",
    };
    html.push_str("<header class=\"masthead\">\n<div class=\"wrap\">\n");
    html.push_str("<h1 class=\"mark\">bakutrend</h1>\n");
    html.push_str(&format!(
        "<p class=\"health {health}\"><span class=\"dot\" aria-hidden=\"true\"></span>{} · {}</p>\n",
        esc(&t.health(view.sources_ok, view.sources_total)),
        esc(&polled),
    ));
    // Every age on the page — a story's, a measurement's, the poll's — is measured from the
    // moment the ranking was read. A snapshot states that moment, because a reader arriving a
    // day later would otherwise read them against their own clock.
    if let Links::Snapshot { built, .. } = links {
        html.push_str(&format!(
            "<p class=\"snapshot\">{} {}</p>\n",
            esc(t.snapshot_of),
            esc(built),
        ));
    }
    // The window is a switch between three states and the language is the locale of the chrome,
    // so they stand together in the masthead, apart from the list they change.
    html.push_str("<ul class=\"row windows\" role=\"list\">\n");
    for window in Window::all() {
        html.push_str(&format!(
            "<li><a href=\"{}\"{}>{}</a></li>\n",
            esc(&Link::of(view, links).window(window).path()),
            active(window == view.window),
            esc(t.window(window)),
        ));
    }
    html.push_str("</ul>\n");
    html.push_str(&format!(
        "<nav aria-label=\"{}\">\n<ul class=\"row langs\" role=\"list\">\n",
        esc(t.language)
    ));
    for lang in Lang::ALL {
        html.push_str(&format!(
            "<li><a href=\"{}\" lang=\"{}\"{}>{}</a></li>\n",
            esc(&Link::of(view, links).lang(lang).path()),
            lang.code(),
            active(lang == view.lang),
            esc(lang.name()),
        ));
    }
    html.push_str("</ul>\n</nav>\n</div>\n</header>\n");

    html.push_str("<div class=\"wrap\">\n");
    // The count is what the window holds, or what the filter left of it: the two numbers are
    // different facts and the sentence says which one is on screen.
    let count = match view.window_total {
        0 => t.stories(view.stories.len()),
        total if total == view.stories.len() => t.stories(total),
        total => t.matching(view.stories.len(), total, t.window(view.window)),
    };
    html.push_str(&format!("<p class=\"count\">{}</p>\n", esc(&count)));

    if let Some((text, level)) = notice(view, "") {
        let class = match level {
            Level::Warn => "notice warn",
            Level::Plain => "notice",
        };
        html.push_str(&format!("<p class=\"{class}\">{}</p>\n", esc(&text)));
    }
    // A failure gets its own line, in the failure colour, whatever is wrong with the window.
    if !view.status.is_empty() {
        html.push_str(&format!(
            "<p class=\"fail\" role=\"alert\">{}</p>\n",
            esc(view.status)
        ));
    }

    html.push_str("<div class=\"controls\">\n");
    html.push_str(&format!(
        "<p class=\"order\">{}</p>\n",
        esc(&t.sorted(t.sort(view.sort)))
    ));
    html.push_str("<ul class=\"row chips\" role=\"list\">\n");
    for sort in Sort::ALL {
        html.push_str(&format!(
            "<li><a href=\"{}\"{}>{}</a></li>\n",
            esc(&Link::of(view, links).sort(sort).path()),
            active(sort == view.sort),
            esc(t.sort(sort)),
        ));
    }
    html.push_str("</ul>\n");
    // One form, submitted without a script: the empty box is what clears the filter. A snapshot
    // has none: there is nothing behind it to run a search, and a box that reloaded the same file
    // would answer a reader with an unchanged page.
    if matches!(links, Links::Query) {
        html.push_str("<form class=\"filter\" method=\"get\" action=\"/\">\n");
        html.push_str(&format!(
            "<input type=\"hidden\" name=\"window\" value=\"{}\">\n",
            view.window.label()
        ));
        html.push_str(&format!(
            "<input type=\"hidden\" name=\"sort\" value=\"{}\">\n",
            sort_slug(view.sort)
        ));
        html.push_str(&format!(
            "<input type=\"hidden\" name=\"lang\" value=\"{}\">\n",
            view.lang.code()
        ));
        html.push_str(&format!(
            "<input type=\"search\" name=\"filter\" value=\"{}\" aria-label=\"{}\">\n",
            esc(view.filter),
            esc(t.filter_label)
        ));
        html.push_str(&format!(
            "<button type=\"submit\">{}</button>\n</form>\n",
            esc(t.filter_label)
        ));
    }
    html.push_str("</div>\n</div>\n");

    html.push_str("<main class=\"wrap\">\n");
    ranked(&mut html, view, selected, links);
    html.push_str("<section class=\"card\" id=\"card\">\n");
    match view.stories.get(selected) {
        Some(story) => card(&mut html, view, story),
        None => {
            // An empty list is not always good news: the status line carries any failure. Only a
            // run that has not polled yet may claim a poll is in progress.
            let message = if view.last_poll.is_none() {
                t.empty_first_run
            } else {
                t.empty_window_long
            };
            html.push_str(&format!("<p class=\"dim\">{}</p>\n", esc(message)));
        }
    }
    html.push_str("</section>\n</main>\n</body>\n</html>\n");
    html
}

/// The ranked list: one row per story, in the order the header names.
fn ranked(html: &mut String, view: &View<'_>, selected: usize, links: Links<'_>) {
    let t = view.lang.strings();
    // The last column shows what the active order is about: a list sorted by views with an age
    // column would explain nothing about why the rows are in that order.
    let last = match view.sort {
        Sort::Growth => t.col_growth,
        Sort::Views => t.col_views,
        _ => t.col_age,
    };

    // The index order is the one the list is set by: the typography scale below applies only when
    // the first row is genuinely the most prominent story, and not merely the first row.
    let ordered = if view.sort == Sort::Index {
        " by-index"
    } else {
        ""
    };
    html.push_str(&format!(
        "<table class=\"ranked{ordered}\">\n<thead>\n<tr>\n"
    ));
    let head = |html: &mut String, label: &str, class: &str| {
        html.push_str(&format!(
            "<th scope=\"col\" class=\"{class}\">{}</th>\n",
            esc(label)
        ));
    };
    head(html, t.col_rank, "rank");
    head(html, t.col_headline, "headline");
    head(html, t.col_outlets, "num");
    // A column of dashes teaches nothing: without a stored ranking to compare against, the
    // movement column is absent rather than empty.
    if view.deltas.is_some() {
        head(html, t.col_status, "status");
    }
    head(html, last, "num");
    html.push_str("</tr>\n</thead>\n<tbody>\n");

    for (index, story) in view.stories.iter().enumerate() {
        let current = if index == selected {
            " aria-current=\"true\""
        } else {
            ""
        };
        html.push_str(&format!("<tr{current}>\n"));
        html.push_str(&format!("<td class=\"rank\">{:02}</td>\n", index + 1));
        // A row leads to the card that explains it, and a snapshot holds no such page: the
        // headline is text there, and the card it does carry explains the top story.
        let headline = match Link::of(view, links).story(index).card_path() {
            Some(href) => format!("<a href=\"{}\">{}</a>", esc(&href), esc(&story.title)),
            None => format!("<span>{}</span>", esc(&story.title)),
        };
        html.push_str(&format!("<td class=\"headline\">{headline}</td>\n"));
        html.push_str(&format!("<td class=\"num\">{}</td>\n", story.outlets.len()));
        if let Some(deltas) = view.deltas {
            let (text, class) = movement(t, deltas.get(&story.key).copied());
            html.push_str(&format!(
                "<td class=\"status {class}\">{}</td>\n",
                esc(&text)
            ));
        }
        let cell = match view.sort {
            Sort::Growth => t.growth(best_relative(story)),
            Sort::Views => compact(story.view_count),
            _ => t.age((view.now - story.updated_at).max(0)),
        };
        html.push_str(&format!("<td class=\"num\">{}</td>\n", esc(&cell)));
        html.push_str("</tr>\n");
    }
    html.push_str("</tbody>\n</table>\n");
}

/// One story: what it carries, and the four inputs behind its score.
fn card(html: &mut String, view: &View<'_>, story: &ScoredStory) {
    let t = view.lang.strings();
    html.push_str(&format!("<h2>{}</h2>\n", esc(&story.title)));
    // A story nobody stored any text for says so, rather than showing an empty block.
    if story.description.is_none() && view.bodies.is_empty() {
        html.push_str(&format!(
            "<p class=\"lede dim\">{}</p>\n",
            esc(t.body_missing)
        ));
    }
    if let Some(description) = &story.description {
        html.push_str(&format!("<p class=\"lede\">{}</p>\n", esc(description)));
    }

    html.push_str(&format!(
        "<h3>{}</h3>\n<p class=\"score\"><b>{:.2}</b> {}</p>\n",
        esc(t.section_score),
        story.score,
        bar(story.score, "bar wide"),
    ));
    // A movement claim needs the previous ranking to exist. Without one, saying `new` would be a
    // claim about a comparison that never happened.
    if let Some(deltas) = view.deltas {
        let delta = deltas.get(&story.key).copied();
        html.push_str(&format!(
            "<p class=\"moved\">{}</p>\n",
            esc(&t.moved(delta, t.window(view.window)))
        ));
    }

    html.push_str(&format!(
        "<h3>{}</h3>\n<dl class=\"signals\">\n",
        esc(t.section_signals)
    ));
    let (pace, measured) = match best_velocity(story) {
        Some(outlet) => match outlet.velocity {
            Some(velocity) => (
                t.pace(velocity.per_hour, outlet.relative_velocity),
                t.velocity_note(velocity.basis, (view.now - velocity.observed_at).max(0)),
            ),
            None => (t.no_views.to_string(), t.velocity_none.to_string()),
        },
        None => (t.no_views.to_string(), t.velocity_none.to_string()),
    };
    signal(
        html,
        t.coverage,
        story.coverage_norm,
        &[&t.outlets(independent_outlets(story))],
    );
    // The kind of number and how old it is, beside the number: an estimate and a measurement are
    // different claims, and a count nobody has refreshed for an hour is not today's pace.
    signal(
        html,
        t.engagement,
        story.engagement_norm,
        &[&pace, &measured],
    );
    signal(
        html,
        t.freshness,
        story.freshness,
        &[&t.ago_sentence((view.now - story.updated_at).max(0))],
    );
    signal(
        html,
        t.spread,
        story.spread_velocity_norm,
        &[&t.picked_up(story.spread_velocity as i64, view.window.spread_seconds())],
    );
    html.push_str("</dl>\n");

    html.push_str(&format!(
        "<h3>{}</h3>\n<ul class=\"sources\" role=\"list\">\n",
        esc(t.section_sources)
    ));
    for outlet in &story.outlets {
        source(html, view, outlet);
    }
    html.push_str("</ul>\n");
}

/// One outlet carrying the story: how it carries it, what its post measured, and the words it
/// published. The text stands under the name of the outlet that wrote it, never under another's.
fn source(html: &mut String, view: &View<'_>, outlet: &OutletContribution) {
    let t = view.lang.strings();
    let age = view.now - outlet.newest;
    html.push_str("<li>\n");
    html.push_str(&format!(
        "<p class=\"src\"><span class=\"outlet\">{}</span> <span class=\"age\">{}</span></p>\n",
        esc(&outlet.outlet),
        esc(&t.ago(age.max(0)))
    ));
    let measured = match (outlet.velocity, outlet.views) {
        (Some(velocity), views) => format!(
            "{} · {} · {} · {}",
            t.provenance(outlet.provenance),
            views.map_or_else(|| t.no_views.to_string(), |views| t.views(views)),
            t.pace(velocity.per_hour, outlet.relative_velocity),
            t.velocity_note(velocity.basis, (view.now - velocity.observed_at).max(0))
        ),
        (None, Some(views)) => format!("{} · {}", t.provenance(outlet.provenance), t.views(views)),
        (None, None) => format!("{} · {}", t.provenance(outlet.provenance), t.no_views),
    };
    html.push_str(&format!("<p class=\"meta\">{}</p>\n", esc(&measured)));
    html.push_str(&format!(
        "<p class=\"src-title\">{}</p>\n",
        match href(&outlet.url) {
            Some(url) => format!(
                "<a href=\"{}\" rel=\"noreferrer\">{}</a>",
                esc(url),
                esc(&outlet.title)
            ),
            None => esc(&outlet.title),
        }
    ));
    html.push_str(&format!("<p class=\"addr\">{}</p>\n", esc(&outlet.url)));
    if let Some(body) = view.bodies.get(&outlet.url) {
        html.push_str(&format!(
            "<details>\n<summary>{} · {}</summary>\n<p>{}</p>\n</details>\n",
            esc(t.section_content),
            esc(&outlet.outlet),
            esc(body)
        ));
    }
    html.push_str("</li>\n");
}

/// One input behind the score: its name, its real normalised value as a bar, and the raw number
/// the normalisation came from.
fn signal(html: &mut String, label: &str, value: f64, notes: &[&str]) {
    html.push_str(&format!(
        "<dt>{}</dt>\n<dd>{} <b>{value:.2}</b>{}</dd>\n",
        esc(label),
        bar(value, "bar"),
        notes
            .iter()
            .map(|note| format!(" <span class=\"note\">{}</span>", esc(note)))
            .collect::<String>(),
    ));
}

/// The movement cell: how far the story moved since the previous window of the same length. The
/// sign carries the fact, so the colour is decoration.
fn movement(t: &Strings, delta: Option<i64>) -> (String, &'static str) {
    match delta {
        Some(movement) if movement > 0 => (format!("+{movement}"), "up"),
        Some(movement) if movement < 0 => (movement.to_string(), "down"),
        Some(_) => ("0".to_string(), "flat"),
        None => (t.new_label.to_string(), "new"),
    }
}

/// A bar of a real normalised value. Decoration: the number it stands for is written beside it,
/// so a reader who cannot see it loses nothing.
fn bar(value: f64, class: &str) -> String {
    format!(
        "<span class=\"{class}\" aria-hidden=\"true\"><i style=\"width:{:.0}%\"></i></span>",
        value.clamp(0.0, 1.0) * 100.0
    )
}

/// The marker on the control that is in force. It is an attribute, not a colour: the styling
/// below is for the eye, and this is what a screen reader announces.
fn active(current: bool) -> &'static str {
    if current {
        " aria-current=\"page\" class=\"on\""
    } else {
        ""
    }
}

/// A link target, or `None` for anything that is not a web address. The URLs come from feeds and
/// channels, and `open_in_browser` refuses the same ones: a `javascript:` URL in an `href` runs
/// in the reader's page.
fn href(url: &str) -> Option<&str> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url)
    } else {
        None
    }
}

/// Text as HTML. Every string on this page came from a feed, a channel, a database or a config
/// file, and none of them is trusted to be markup.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for glyph in text.chars() {
        match glyph {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(glyph),
        }
    }
    out
}

/// A query value. Percent-encoding the bytes of the UTF-8 string keeps a filter with a space, an
/// `&` or an Azerbaijani letter inside one parameter.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The page's palette, type and spacing, in one place.
///
/// Colour carries meaning only where there is meaning to carry, and the meanings are the
/// terminal's: cyan is the index and the control in force, green a story that was not in the
/// previous ranking, amber a window that is not showing what its tab claims, red a failure.
/// `light-dark` keeps both themes in one list of names, so a colour cannot be changed in one
/// theme and forgotten in the other.
const CSS: &str = r##"
:root {
  color-scheme: light dark;
  --bg: light-dark(oklch(.981 .004 240), oklch(.176 .011 245));
  --surface: light-dark(oklch(1 0 0), oklch(.218 .012 245));
  --ink: light-dark(oklch(.23 .014 250), oklch(.94 .006 245));
  --muted: light-dark(oklch(.505 .013 250), oklch(.72 .014 245));
  --rule: light-dark(oklch(.906 .006 245), oklch(.3 .012 245));
  --accent: light-dark(oklch(.5 .085 208), oklch(.76 .095 200));
  --on-accent: light-dark(oklch(.99 .005 220), oklch(.22 .02 240));
  --new: light-dark(oklch(.5 .1 155), oklch(.78 .11 152));
  --warn: light-dark(oklch(.5 .1 75), oklch(.8 .1 82));
  --fail: light-dark(oklch(.5 .17 25), oklch(.72 .13 25));
  /* The selected row and the row the pointer is on. Tints of the accent, so the two say the
     same thing at two strengths instead of two unrelated colours. */
  --here: color-mix(in oklab, var(--accent) 10%, var(--bg));
  --hover: color-mix(in oklab, var(--accent) 5%, var(--bg));

  /* One spacing scale in 0.25rem steps, one radius and one type scale. Every value below comes
     from them, so nothing here is a number picked by eye. */
  --s1: .25rem; --s2: .5rem; --s3: .75rem; --s4: 1rem;
  --s5: 1.25rem; --s6: 1.5rem; --s8: 2rem; --s12: 3rem;
  --r: 6px; --r-sm: 4px;

  /* Two faces, two jobs: words in the interface face, every measurement — a rank, a count, a
     score, a rate, an address — in the data face, whose digits are all one width. Nothing is
     downloaded; the page is served from anything that can serve HTML, so it uses the faces the
     reader's system already has. Nothing is upper-cased in CSS either: Azerbaijani's capital
     `i` is `İ`, and no case mapping can know that. The words that must be upper case arrive
     that way from `Strings`. */
  --ui: system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
  --data: ui-monospace, "SF Mono", SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
  --t-micro: .6875rem; --t-small: .8125rem; --t-body: .9375rem; --t-lead: 1.0625rem;
  --t-top: clamp(1.25rem, 1.05rem + .9vw, 1.6rem);
  --t-h2: clamp(1.3rem, 1.05rem + 1.1vw, 1.7rem);
  --t-score: clamp(1.75rem, 1.35rem + 1.4vw, 2.25rem);
  --meter: 7rem;
}

* { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--bg); color: var(--ink);
  font: var(--t-body)/1.5 var(--ui);
  -webkit-text-size-adjust: 100%;
}
.wrap { max-width: 76rem; margin-inline: auto; padding-inline: var(--s4); }
::selection { background: color-mix(in oklab, var(--accent) 26%, transparent); }
a { color: var(--accent); text-decoration-thickness: 1px; text-underline-offset: .16em; }
a:focus-visible, button:focus-visible, input:focus-visible, summary:focus-visible {
  outline: 2px solid var(--accent); outline-offset: 2px;
}
h1, h2, h3, p, dl, dd, ul { margin: 0; }
ul.row { display: flex; flex-wrap: wrap; gap: var(--s1); list-style: none; padding: 0; }

/* The masthead carries what the instrument is doing — the wordmark, the health of the poll, the
   window and the language — and stays at the top, because the page is made to be left open while
   the poller runs. */
.masthead {
  position: sticky; top: 0; z-index: 2;
  background: color-mix(in oklab, var(--bg) 86%, transparent);
  backdrop-filter: blur(12px);
  border-bottom: 1px solid var(--rule);
}
.masthead .wrap {
  display: flex; flex-wrap: wrap; align-items: center; gap: var(--s2) var(--s4);
  padding-block: var(--s3);
}
.mark {
  font: 600 1rem/1.2 var(--data);
  letter-spacing: -.02em;
}
/* The dot is the poll's state, and the words beside it say the same thing: nothing here rests on
   a colour. */
.health {
  display: flex; align-items: center; gap: var(--s2);
  margin-inline-end: auto;
  font-size: var(--t-small); color: var(--muted);
}
.dot { flex: none; width: .5rem; height: .5rem; border-radius: 50%; background: var(--muted); }
.health.ok .dot { background: var(--accent); }
.health.warn .dot { background: var(--warn); }
/* No cycle has finished: nothing has been observed, so the dot claims nothing either. */
.health.off .dot { background: var(--muted); }
/* The moment a static page's ranking was read. It stands with the health line it qualifies: the
   poll age beside it is measured from that moment, not from the reader's. */
.snapshot { font: var(--t-micro)/1.4 var(--data); color: var(--muted); }

/* The window is a switch between three states, so it is drawn as one. */
.windows {
  display: inline-flex; flex-wrap: nowrap; gap: 2px; padding: 2px;
  background: var(--surface); border: 1px solid var(--rule); border-radius: var(--r);
}
.windows a {
  font: var(--t-small)/1.4 var(--data); color: var(--muted); text-decoration: none;
  padding: .2rem .6rem; border-radius: var(--r-sm);
}
.windows a:hover { color: var(--ink); }
/* A control in force is filled with the accent — the terminal's own rule, and the one thing the
   two front ends cannot disagree about. */
.windows a.on, .chips a.on {
  background: var(--accent); color: var(--on-accent); font-weight: 600;
}
.windows a.on:hover, .chips a.on:hover { color: var(--on-accent); }

.chips { display: flex; flex-wrap: wrap; gap: var(--s1); list-style: none; padding: 0; }
.chips a {
  display: inline-block; padding: .2rem .6rem;
  font-size: var(--t-small); color: var(--ink); text-decoration: none;
  background: var(--surface); border: 1px solid var(--rule); border-radius: 999px;
}
.chips a:hover { color: var(--accent); border-color: var(--accent); }
/* The language is a locale, not a mode of the instrument: it changes the words of the chrome and
   never the ranking, so it is the smallest control on the page, marked with ink rather than with
   the accent the window and the order carry. */
.langs a {
  display: inline-block; padding: .15rem .5rem; border-radius: 999px;
  border: 1px solid transparent; font-size: var(--t-micro);
  color: var(--muted); text-decoration: none;
}
.langs a:hover { color: var(--ink); }
.langs a.on { color: var(--ink); border-color: var(--rule); font-weight: 600; }
.langs a.on:hover { color: var(--ink); }

/* WCAG 2.2 target size: a control is at least 24 by 24 CSS px, and a fingertip gets 44. */
.chips a, .langs a, .windows a, .filter button, .filter input[type=search] { min-height: 1.5rem; }
summary { min-height: 1.5rem; }
@media (pointer: coarse) {
  .chips a, .langs a, .windows a, .filter button, summary { min-height: 2.75rem; padding-block: var(--s2); }
}

/* What the window holds, and what the poller could not read, above the list they describe. A
   plain notice is one line of text; a warning or a failure is a box, because those are the ones
   that need to be found. */
.count { margin-top: var(--s5); font-size: var(--t-lead); }
.notice, .fail {
  display: flex; align-items: baseline; gap: var(--s2);
  margin-top: var(--s3); font-size: var(--t-small); color: var(--muted);
}
.notice::before, .fail::before {
  content: ""; flex: none; width: .45rem; height: .45rem; border-radius: 50%;
  background: currentColor; align-self: center;
}
.notice.warn, .fail {
  padding: var(--s2) var(--s3);
  border: 1px solid var(--rule); border-radius: var(--r-sm);
}
.notice.warn {
  color: var(--warn); background: color-mix(in oklab, var(--warn) 8%, var(--bg));
  border-color: color-mix(in oklab, var(--warn) 30%, var(--rule));
}
.fail {
  color: var(--fail); background: color-mix(in oklab, var(--fail) 8%, var(--bg));
  border-color: color-mix(in oklab, var(--fail) 30%, var(--rule));
}

/* How the list is arranged, and the one field the reader can type in. */
.controls {
  display: flex; flex-wrap: wrap; align-items: center; gap: var(--s2) var(--s4);
  margin-top: var(--s4); padding-bottom: var(--s3);
  border-bottom: 1px solid var(--rule);
}
.order { font-size: var(--t-small); color: var(--muted); }
.filter { display: flex; gap: var(--s1); margin-inline-start: auto; }
.filter input[type=search], .filter button {
  font: var(--t-small)/1.4 var(--ui);
  color: var(--ink); background: var(--surface);
  border: 1px solid var(--rule); border-radius: var(--r-sm);
  padding: .25rem .5rem;
}
.filter input[type=search] { width: 13rem; appearance: none; -webkit-appearance: none; }
.filter button { font-weight: 600; cursor: pointer; }
.filter button:hover { color: var(--accent); border-color: var(--accent); }

main {
  display: grid; grid-template-columns: minmax(0, 1.7fr) minmax(0, 1fr);
  gap: var(--s8); align-items: start;
  padding-block: var(--s5) var(--s12);
}
@media (max-width: 62rem) { main { grid-template-columns: minmax(0, 1fr); gap: var(--s6); } }

/* The ranked list. The browser sizes the columns: the widest header in the active language sets
   the number columns, and the headline takes what is left and breaks rather than pushing them
   out. Hand-set widths had to be re-tuned per language and still let `OUTLETS` spill into the
   next column. */
table.ranked { border-collapse: collapse; width: 100%; }
table.ranked th {
  text-align: left; white-space: nowrap;
  padding: var(--s2) var(--s3) var(--s1);
  font-size: var(--t-micro); font-weight: 600; letter-spacing: .09em;
  color: var(--muted); border-bottom: 1px solid var(--rule);
}
/* `--pad` is the row's own padding, so the headline link can carry it and the whole row height
   becomes the target. The negative margin keeps the line box where it was: a taller target, not
   a taller row. */
table.ranked td {
  --pad: var(--s2);
  padding: var(--pad) var(--s3);
  border-bottom: 1px solid var(--rule);
  vertical-align: baseline;
}
table.ranked tbody tr:hover td { background: var(--hover); }
table.ranked tbody tr[aria-current] td { background: var(--here); }
/* The sign of a movement carries the fact; the colour is decoration. */
td.rank, td.num, td.status, .age {
  font-family: var(--data); font-variant-numeric: tabular-nums; white-space: nowrap;
}
td.rank { font-size: var(--t-small); color: var(--muted); }
td.num { font-size: var(--t-small); text-align: right; }
td.status { font-size: var(--t-small); }
td.headline { overflow-wrap: break-word; }
/* A headline is a link to its card, or the text of it in a snapshot, which holds no card per
   row. One set of type rules covers both. */
td.headline :is(a, span) {
  display: inline-block; padding-block: var(--pad); margin-block: calc(-1 * var(--pad));
  font-size: var(--t-body); font-weight: 500; line-height: 1.35;
  color: var(--ink); text-decoration: none; text-wrap: pretty;
}
td.headline a:hover { text-decoration: underline; text-decoration-color: var(--accent); }
tr[aria-current] td:first-child { box-shadow: inset 3px 0 0 0 var(--accent); }
tr[aria-current] .rank { color: var(--accent); font-weight: 700; }
.status.new { color: var(--new); font-weight: 700; }
.status.up { color: var(--accent); }
.status.flat { color: var(--muted); }

/* Ordered by the index, the list is set the way the index ranks it: the stories at the top are
   the ones the country is reading, so they are the ones set largest. Under any other order the
   first row is only the first row, and every row is set the same. */
.ranked.by-index tbody tr:nth-child(-n+3) { --pad: var(--s3); }
.ranked.by-index tbody tr:nth-child(-n+3) .headline :is(a, span) {
  font-size: var(--t-top); font-weight: 640; line-height: 1.22; letter-spacing: -.012em;
}
.ranked.by-index tbody tr:nth-child(n+4):nth-child(-n+10) .headline :is(a, span) {
  font-size: var(--t-lead); font-weight: 560; line-height: 1.3;
}

/* The card: one story, and the four inputs behind its score. */
.card {
  background: var(--surface); border: 1px solid var(--rule); border-radius: var(--r);
  padding: var(--s5);
  /* Choosing a row jumps here, so the header cannot scroll over the story it explains. */
  scroll-margin-top: 3.5rem;
}
.card h2 {
  font-size: var(--t-h2); font-weight: 640; line-height: 1.25; letter-spacing: -.014em;
  text-wrap: pretty;
}
.card > h3 {
  margin-top: var(--s6); padding-top: var(--s4); border-top: 1px solid var(--rule);
  font-size: var(--t-micro); font-weight: 600; letter-spacing: .09em; color: var(--muted);
}
.lede { margin-top: var(--s2); font-size: var(--t-lead); line-height: 1.55; text-wrap: pretty; }
.dim { color: var(--muted); }
.score { display: flex; align-items: center; gap: var(--s3); margin-top: var(--s3); }
.score b {
  font: 500 var(--t-score)/1 var(--data);
  letter-spacing: -.02em; color: var(--accent);
}
.score .bar { flex: 1; height: .625rem; }
.moved { margin-top: var(--s2); font-size: var(--t-small); color: var(--muted); }

/* A meter is a real normalised value on a track that is the same width for all four inputs, so
   the four can be compared by eye. The value is written beside it, so a reader who cannot see
   the fill loses nothing. */
.bar {
  display: block; height: .5rem; overflow: hidden;
  background: color-mix(in oklab, var(--rule) 75%, transparent);
  border-radius: 999px;
}
.bar i {
  display: block; height: 100%; background: var(--accent); border-radius: 999px;
  transform-origin: left center; animation: meter .55s cubic-bezier(.22, .8, .2, 1) both;
}
@keyframes meter { from { transform: scaleX(0); } }

dl.signals {
  display: grid; grid-template-columns: 5.25rem minmax(0, 1fr);
  gap: var(--s3) var(--s3); align-items: center; margin-top: var(--s3);
}
dl.signals dt { font-size: var(--t-small); color: var(--muted); }
dl.signals dd {
  display: grid; grid-template-columns: var(--meter) minmax(0, 1fr);
  gap: var(--s1) var(--s2); align-items: center;
}
dl.signals dd .bar { width: var(--meter); }
dl.signals dd b { font: 600 var(--t-small)/1 var(--data); }
dl.signals dd .note {
  grid-column: 1 / -1; font-size: var(--t-small); color: var(--muted);
}

/* One outlet per publication: who carried it and when, what it measured, the wording it ran,
   and the address — the text always under the name of the outlet that wrote it. */
.sources { list-style: none; padding: 0; margin-top: var(--s1); }
.sources li {
  display: grid; gap: var(--s2);
  padding: var(--s3) 0; border-top: 1px solid var(--rule);
}
.sources li:first-child { border-top: 0; padding-top: var(--s2); }
.sources .src { display: flex; align-items: baseline; justify-content: space-between; gap: var(--s3); }
.sources .outlet { font-weight: 600; }
.sources .age { font-size: var(--t-small); color: var(--muted); }
.sources .meta { font: var(--t-small)/1.45 var(--data); color: var(--muted); }
.sources .src-title { font-weight: 500; }
.sources .src-title a {
  color: var(--ink); text-decoration: underline;
  text-decoration-color: var(--rule); text-underline-offset: .18em;
}
.sources .src-title a:hover { text-decoration-color: var(--accent); }
.sources .addr { font: var(--t-small)/1.45 var(--data); color: var(--muted); overflow-wrap: anywhere; }
details { margin-top: var(--s1); }
summary {
  display: inline-flex; align-items: center; gap: var(--s1);
  font-size: var(--t-small); font-weight: 600; color: var(--muted); cursor: pointer;
}
summary::-webkit-details-marker { display: none; }
summary::before { content: "▸"; font-size: .8em; transition: transform .18s ease; }
details[open] summary::before { transform: rotate(90deg); }
details p { margin-top: var(--s2); white-space: pre-wrap; overflow-wrap: anywhere; }

@media (max-width: 34rem) {
  :root { --meter: 5rem; }
  table.ranked th, table.ranked td { padding-inline: var(--s1); }
  .card { padding: var(--s4); scroll-margin-top: 6rem; }
  .filter { flex: 1 1 100%; margin-inline-start: 0; }
  .filter input[type=search] { width: 100%; }
}
/* A narrow screen keeps the header to two rows: the wordmark and the poll beside it, then the
   window and the language, rather than four rows of chrome over the list. */
@media (max-width: 48rem) {
  .health { margin-inline-end: 0; }
}

/* Where a change of window, order or story is a new page, the browser can carry the reader
   across instead of flashing white: no script, and the reader who asked for less motion keeps
   the plain navigation. */
@view-transition { navigation: auto; }
@media (prefers-reduced-motion: no-preference) {
  .health.ok .dot { animation: live 2.4s ease-in-out infinite; }
  @keyframes live { 50% { opacity: .35; } }
}
@media (prefers-reduced-motion: reduce) {
  *, ::before, ::after { animation: none !important; transition: none !important; }
  ::view-transition-group(*), ::view-transition-old(root), ::view-transition-new(root) {
    animation: none !important;
  }
}
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Mode, Sort};
    use crate::score::{Provenance, Velocity};
    use std::collections::{HashMap, HashSet};

    fn outlet(name: &str, url: &str, measured: bool) -> OutletContribution {
        OutletContribution {
            outlet: name.into(),
            weight: 1.0,
            provenance: Provenance::Independent,
            newest: 0,
            views: Some(2650),
            velocity: Some(Velocity {
                per_hour: 900.0,
                basis: if measured {
                    crate::score::VelocityBasis::Measured
                } else {
                    crate::score::VelocityBasis::Estimated
                },
                observed_seconds: 3600,
                observed_at: 0,
            }),
            relative_velocity: Some(2.0),
            title: format!("{name} headline"),
            url: url.into(),
        }
    }

    fn story(title: &str) -> ScoredStory {
        ScoredStory {
            key: title.into(),
            title: title.into(),
            description: Some("Bir xəbərin qısa təsviri.".into()),
            score: 0.91,
            coverage: 3.0,
            coverage_norm: 1.0,
            engagement: 1.2,
            engagement_norm: 0.8,
            freshness: 0.7,
            engagement_basis: Some(crate::score::VelocityBasis::Measured),
            engagement_observed_at: Some(0),
            spread: 4,
            spread_velocity: 3.0,
            spread_velocity_norm: 1.0,
            started_at: 0,
            updated_at: 0,
            view_count: 2650,
            newest: 0,
            outlets: vec![outlet(
                "Qafqazinfo",
                "https://qafqazinfo.az/news/detail/x-1",
                true,
            )],
        }
    }

    /// One `View` over the given stories, with everything the page reads set to a known value.
    /// A test changes the one field it is about.
    fn view<'a>(
        stories: &'a [ScoredStory],
        bodies: &'a HashMap<String, String>,
        deltas: Option<&'a HashMap<String, i64>>,
    ) -> View<'a> {
        View {
            lang: Lang::En,
            window: Window::Day,
            stories,
            bodies,
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
            sources_ok: Some(19),
            sources_total: 19,
            last_poll: Some(0),
            degraded: &[],
            now: 3600,
            status: "",
        }
    }

    /// The page as a reader gets it, over the given stories, as the server renders it.
    fn render(
        stories: &[ScoredStory],
        bodies: &HashMap<String, String>,
        deltas: Option<&HashMap<String, i64>>,
    ) -> String {
        page(&view(stories, bodies, deltas), Links::Query)
    }

    #[test]
    fn the_page_carries_what_the_ranking_produced() {
        let stories = vec![story("Bakıda bu yollar bağlıdır")];
        let bodies = HashMap::new();
        let html = render(&stories, &bodies, None);
        assert!(html.contains("Bakıda bu yollar bağlıdır"), "{html}");
        assert!(html.contains("0.91"), "the score is on the page: {html}");
        assert!(html.contains("19/19 sources ok"), "{html}");
        assert!(
            html.contains("Qafqazinfo"),
            "the source list is there: {html}"
        );
        assert!(
            html.contains("https://qafqazinfo.az/news/detail/x-1"),
            "the article's address is readable: {html}"
        );
        assert!(
            html.contains("Coverage") && html.contains("Spread"),
            "{html}"
        );
    }

    /// Text arrives from a feed, a channel or a database, and none of it is markup. A headline
    /// carried into the page as-is would run in the reader's browser.
    #[test]
    fn text_from_a_feed_is_escaped() {
        let mut hostile = story("<script>alert('x')</script> & \"quoted\"");
        hostile.key = hostile.title.clone();
        hostile.description = Some("</p><img src=x onerror=alert(1)>".into());
        hostile.outlets[0].title = "<b>bold</b>".into();
        let stories = vec![hostile];
        let bodies = HashMap::new();
        let html = render(&stories, &bodies, None);
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("<b>bold"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("&amp;"), "{html}");
    }

    /// A source whose locator is not a web address gets no link, so a `javascript:` URL cannot
    /// become something a reader clicks.
    #[test]
    fn a_source_that_is_not_a_web_address_is_not_a_link() {
        let mut hostile = story("Adi başlıq");
        hostile.outlets[0].url = "javascript:alert(1)".into();
        let stories = vec![hostile];
        let bodies = HashMap::new();
        let html = render(&stories, &bodies, None);
        assert!(!html.contains("href=\"javascript"), "{html}");
        assert!(
            html.contains("javascript:alert(1)"),
            "shown as text: {html}"
        );
    }

    /// A filter with a space, an `&` or an Azerbaijani letter has to survive the round trip
    /// through a URL, and the link that carries it must not end the attribute early.
    #[test]
    fn a_filter_travels_inside_one_parameter() {
        let stories = vec![story("Bakıda bu yollar bağlıdır")];
        let bodies = HashMap::new();
        let mut view = view(&stories, &bodies, None);
        view.filter = "külək & yağış";
        let html = page(&view, Links::Query);
        assert!(
            html.contains("filter=k%C3%BCl%C9%99k%20%26%20ya%C4%9F%C4%B1%C5%9F"),
            "{html}"
        );
        view.window = Window::Week;
        assert!(
            page(&view, Links::Query).contains("window=7d"),
            "a link carries the window it was made in"
        );
    }

    /// A snapshot has no server behind it. Its controls name files, it carries no filter form,
    /// and a row is text rather than a link to a card no page holds. It also states when it was
    /// built, because every age on it is measured from that moment.
    #[test]
    fn a_snapshot_links_files_where_the_server_links_queries() {
        let stories = vec![story("Bakıda bu yollar bağlıdır"), story("İkinci xəbər")];
        let bodies = HashMap::new();
        let snapshot = |prefix: &'static str| {
            page(
                &view(&stories, &bodies, None),
                Links::Snapshot {
                    prefix,
                    built: "2026-09-15 14:03 UTC",
                },
            )
        };

        let deep = snapshot("../../../");
        assert!(
            deep.contains("href=\"../../../en/7d/index/index.html\""),
            "a window link names a file: {deep}"
        );
        assert!(
            deep.contains("href=\"../../../en/1h/index/index.html\""),
            "a window link names a file: {deep}"
        );
        assert!(
            deep.contains("href=\"../../../en/24h/growth/index.html\""),
            "an order travels the same way, keeping the window: {deep}"
        );
        assert!(
            deep.contains("href=\"../../../az/24h/index/index.html\""),
            "so does a language: {deep}"
        );
        assert!(
            deep.contains("href=\"../../../index.html\""),
            "the default view is the front door: {deep}"
        );
        assert!(!deep.contains("class=\"filter\""), "{deep}");
        assert!(!deep.contains("#card"), "no page holds a card per row");
        assert!(
            deep.contains("<td class=\"headline\"><span>İkinci xəbər</span></td>"),
            "a row in a snapshot is text: {deep}"
        );
        assert!(
            deep.contains("static snapshot of 2026-09-15 14:03 UTC"),
            "{deep}"
        );

        // The root page reaches every other page without climbing, and the server's own page is
        // unchanged: it still links a query and still carries the form.
        assert!(
            snapshot("").contains("href=\"en/24h/chronology/index.html\""),
            "the front door links inwards"
        );
        let served = render(&stories, &bodies, None);
        assert!(served.contains("class=\"filter\""), "{served}");
        assert!(
            served.contains("&amp;story=2#card"),
            "a served row still links its card: {served}"
        );
    }

    /// The page speaks the language it was given, and the chrome is translated rather than the
    /// news: headlines arrive in whichever language their outlet wrote them.
    #[test]
    fn the_page_speaks_the_language_it_was_given() {
        let stories = vec![story("Bakıda bu yollar bağlıdır")];
        let bodies = HashMap::new();
        let mut view = view(&stories, &bodies, None);
        view.lang = Lang::Az;
        let html = page(&view, Links::Query);
        assert!(html.contains("<html lang=\"az\">"), "{html}");
        assert!(html.contains("BAŞLIQ"), "{html}");
        assert!(html.contains("Bakıda bu yollar bağlıdır"), "{html}");
        // Every language is reachable from the page, each named in its own words.
        for name in ["English", "Azərbaycan dili", "Русский"] {
            assert!(
                html.contains(name),
                "{lang} link missing: {html}",
                lang = name
            );
        }
    }

    /// The list says what its order is about: a column of ages under a views order would explain
    /// nothing about why the rows are in that order.
    #[test]
    fn the_last_column_follows_the_order() {
        let stories = vec![story("Bakıda bu yollar bağlıdır")];
        let bodies = HashMap::new();
        let ordered = |sort| {
            let mut view = view(&stories, &bodies, None);
            view.sort = sort;
            page(&view, Links::Query)
        };
        let views = ordered(Sort::Views);
        assert!(views.contains("VIEWS"), "{views}");
        assert!(views.contains("2.6K"), "the compacted count: {views}");
        let growth = ordered(Sort::Growth);
        assert!(growth.contains("GROWTH"), "{growth}");
        assert!(growth.contains("×2.0"), "{growth}");
        let index = ordered(Sort::Index);
        assert!(index.contains("AGE"), "{index}");
        assert!(index.contains("1h"), "{index}");
    }

    /// A failure has to be on the page, and a window that holds one story says so instead of
    /// pretending the hour was quiet.
    #[test]
    fn a_notice_and_a_failure_are_both_shown() {
        let stories = vec![story("Bakıda bu yollar bağlıdır")];
        let bodies = HashMap::new();
        let mut view = view(&stories, &bodies, None);
        view.window_total = 1;
        view.sources_ok = None;
        view.last_poll = None;
        view.status = "database read failed: disk I/O error";
        let html = page(&view, Links::Query);
        assert!(html.contains("1 story in the last 24 hours"), "{html}");
        assert!(html.contains("database read failed"), "{html}");
        assert!(
            html.contains("sources unknown"),
            "no health is claimed: {html}"
        );
        assert!(html.contains("never"), "no poll is claimed: {html}");
    }

    /// The movement column is absent rather than filled with dashes, and a story that was not in
    /// the previous ranking says so in words.
    #[test]
    fn movement_is_shown_only_when_a_ranking_to_compare_with_exists() {
        let stories = vec![story("Bakıda bu yollar bağlıdır")];
        let bodies = HashMap::new();
        let without = render(&stories, &bodies, None);
        assert!(!without.contains(">STATUS<"), "{without}");

        let deltas = HashMap::from([("Bakıda bu yollar bağlıdır".to_string(), 2)]);
        let with = render(&stories, &bodies, Some(&deltas));
        assert!(with.contains(">STATUS<"), "{with}");
        assert!(with.contains("+2"), "{with}");
        assert!(
            with.contains("since the previous 24h window"),
            "the card names what it compares against: {with}"
        );
    }

    /// The stored text of a story stands under the name of the outlet that published it.
    #[test]
    fn the_stored_text_is_shown_per_publication() {
        let stories = vec![story("Bakıda bu yollar bağlıdır")];
        let bodies = HashMap::from([(
            "https://qafqazinfo.az/news/detail/x-1".to_string(),
            "Yolların təmiri sentyabrın sonuna qədər davam edəcək.".to_string(),
        )]);
        let html = render(&stories, &bodies, None);
        assert!(
            html.contains("Yolların təmiri sentyabrın sonuna qədər davam edəcək."),
            "{html}"
        );
        assert!(html.contains("CONTENT · Qafqazinfo"), "{html}");
    }

    /// A window request the program cannot answer is refused, rather than silently becoming the
    /// default window or a list of dashes.
    #[test]
    fn a_window_or_order_the_program_does_not_know_is_a_bad_request() {
        assert_eq!(parse_window(None), Ok(Window::Day));
        assert_eq!(parse_window(Some("1h")), Ok(Window::Hour));
        assert_eq!(parse_window(Some("7d")), Ok(Window::Week));
        assert!(parse_window(Some("30d")).is_err());

        assert_eq!(parse_sort(None), Ok(Sort::Index));
        assert_eq!(parse_sort(Some("chronology")), Ok(Sort::Chronology));
        assert!(parse_sort(Some("popularity")).is_err());

        // Every order the page offers has a name in the URL, and every name maps back to it.
        let named: HashSet<&str> = Sort::ALL.iter().map(|sort| sort_slug(*sort)).collect();
        assert_eq!(named.len(), Sort::ALL.len(), "two orders share a name");
        for sort in Sort::ALL {
            assert_eq!(parse_sort(Some(sort_slug(sort))), Ok(sort));
        }
    }
}
