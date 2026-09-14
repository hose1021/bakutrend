//! Composition root: resolve paths, open the store, then either run the poller alone
//! or run the TUI with the poller on its own thread.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bakutrend::app::{Action, App, AppEvent};
use bakutrend::cli::Cli;
use bakutrend::config::Config;
use bakutrend::dirs::AppDirs;
use bakutrend::poller::{Backoff, poll_once};
use bakutrend::source::http::HttpFetcher;
use bakutrend::source::{SourceKind, SourceSpec};
use bakutrend::store::Store;
use clap::Parser;

const TICK: Duration = Duration::from_millis(250);

fn main() {
    let cli = Cli::parse();
    if let Err(message) = run(cli) {
        eprintln!("Error: {message}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let dirs = AppDirs::resolve();
    let db_path = dirs
        .as_ref()
        .map(AppDirs::db_path)
        .unwrap_or_else(|| "bakutrend.sqlite".into());
    let config_path = cli
        .config
        .clone()
        .or_else(|| dirs.as_ref().map(AppDirs::config_path));

    if cli.reset_db && db_path.exists() {
        std::fs::remove_file(&db_path)?;
        let _ = std::fs::remove_file(db_path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("sqlite-shm"));
    }

    let config = match config_path.as_deref() {
        Some(path) if path.exists() => Config::load(path)?,
        _ => Config::default(),
    };

    if let (Some(level), Some(dirs)) = (cli.log.as_deref(), dirs.as_ref()) {
        init_file_logger(level, &dirs.log_path());
    }

    let mut store = Store::open(&db_path)?;
    for (spec, enabled) in config.source_specs() {
        store.ensure_source(&spec, enabled)?;
        if !enabled {
            let rows = store.sources(false)?;
            if let Some(row) = rows.iter().find(|r| r.locator == spec.locator) {
                store.set_enabled(row.id, false)?;
            }
        }
    }

    // The Google source exists only to seed the week window once. It is created here
    // because it is deliberately absent from the user's configurable source list.
    store.ensure_source(
        &SourceSpec {
            kind: SourceKind::Google,
            outlet: "Google News".to_string(),
            name: "Google News 7d".to_string(),
            locator: "google:7d".to_string(),
        },
        true,
    )?;

    let now = chrono::Utc::now().timestamp();
    if cli.poll_only {
        return run_poller_forever(store, config, now);
    }

    let (tx, rx) = mpsc::channel::<AppEvent>();
    // Pressing `r` nudges the poller rather than waiting out the interval.
    let (nudge_tx, nudge_rx) = mpsc::channel::<()>();

    let input_tx = tx.clone();
    thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if input_tx.send(AppEvent::Input(event)).is_err() {
                break;
            }
        }
    });

    let poll_tx = tx.clone();
    let interval = Duration::from_secs(config.poll_interval_secs.max(30));
    let retention_days = config.retention.view_sample_days;
    thread::spawn(move || {
        let fetcher = match HttpFetcher::new("azerbaycan OR baki OR bakı") {
            Ok(fetcher) => fetcher,
            Err(error) => {
                let _ = poll_tx.send(AppEvent::PollFailed(error.to_string()));
                return;
            }
        };
        let mut store = store;
        let mut backoff = Backoff::new();
        loop {
            let now = chrono::Utc::now().timestamp();
            match poll_once(&mut store, &fetcher, &mut backoff, now, retention_days) {
                Ok(report) => {
                    if poll_tx.send(AppEvent::PollDone(report)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    if poll_tx
                        .send(AppEvent::PollFailed(error.to_string()))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            match nudge_rx.recv_timeout(interval) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    let reader = Store::open(&db_path)?;
    let mut app = App::new(reader, config, now);
    let mut terminal = ratatui::init();
    let result = run_tui(&mut terminal, &mut app, &rx, &nudge_tx);
    ratatui::restore();
    result
}

fn run_tui(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &mpsc::Receiver<AppEvent>,
    nudge: &mpsc::Sender<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        // The app's clock is injected, so the loop advances it once per iteration. Without
        // this, a refresh triggered by a keypress or by a poll result would rank against a
        // stale reference time.
        app.set_now(chrono::Utc::now().timestamp());
        terminal.draw(|frame| {
            let view = app.view();
            bakutrend::ui::draw(frame, &view);
        })?;

        match rx.recv_timeout(TICK) {
            Ok(event) => match app.handle(event) {
                Action::Quit => return Ok(()),
                Action::OpenUrl(url) => open_in_browser(&url),
                Action::ForcePoll => {
                    let _ = nudge.send(());
                }
                Action::None => {}
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn run_poller_forever(
    mut store: Store,
    config: Config,
    _now: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let fetcher = HttpFetcher::new("azerbaycan OR baki OR bakı")?;
    let mut backoff = Backoff::new();
    let interval = Duration::from_secs(config.poll_interval_secs.max(30));
    loop {
        let now = chrono::Utc::now().timestamp();
        match poll_once(
            &mut store,
            &fetcher,
            &mut backoff,
            now,
            config.retention.view_sample_days,
        ) {
            Ok(report) => eprintln!(
                "[{}] ok={} new={} samples={} pruned={} failed={}",
                now,
                report.ok,
                report.new_items,
                report.samples,
                report.pruned,
                report.failed.len()
            ),
            Err(error) => eprintln!("poll failed: {error}"),
        }
        thread::sleep(interval);
    }
}

fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

/// Minimal opt-in file logger, matching the sibling project: off by default, truncated
/// on startup.
fn init_file_logger(level: &str, path: &std::path::Path) {
    struct FileLogger(std::sync::Mutex<std::fs::File>);
    impl log::Log for FileLogger {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            use std::io::Write;
            if let Ok(mut file) = self.0.lock() {
                let _ = writeln!(file, "[{}] {}", record.level(), record.args());
            }
        }
        fn flush(&self) {}
    }

    let parsed = match level.to_ascii_lowercase().as_str() {
        "off" => return,
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Debug,
    };
    // The log lives in the state directory, which may not exist on a first run; without
    // this `File::create` fails and the requested log is silently dropped.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(file) = std::fs::File::create(path) {
        // `set_logger` wants `&'static`, so the logger lives in a `OnceLock` rather than
        // leaking a box.
        static LOGGER: std::sync::OnceLock<FileLogger> = std::sync::OnceLock::new();
        let _ = LOGGER.set(FileLogger(std::sync::Mutex::new(file)));
        if let Some(logger) = LOGGER.get()
            && log::set_logger(logger).is_ok()
        {
            log::set_max_level(parsed);
        }
    }
}
