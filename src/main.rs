//! Composition root: resolve paths, open the store, then either run the poller alone
//! or run the TUI with the poller on its own thread.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bakutrend::app::{Action, App, AppEvent};
use bakutrend::cli::Cli;
use bakutrend::config::Config;
use bakutrend::dirs::AppDirs;
use bakutrend::error::{ConfigError, StoreError};
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
    let default_config_path = dirs.as_ref().map(AppDirs::config_path);

    if cli.reset_db {
        reset_database(&db_path)?;
    }

    let config = resolve_config(cli.config.as_deref(), default_config_path.as_deref())?;

    if let (Some(level), Some(dirs)) = (cli.log.as_deref(), dirs.as_ref()) {
        init_file_logger(level, &dirs.log_path());
    }

    let mut store = Store::open(&db_path)?;
    register_sources(&mut store, &config)?;

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

    // Built here rather than inside the thread: a client that cannot be constructed is a
    // startup failure the user has to see and exit on, not a thread that quietly returns and
    // leaves a TUI that never polls.
    let fetcher = HttpFetcher::new("azerbaycan OR baki OR bakı")?;

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

/// Register every configured source, and let the config file be the single authority for the
/// enabled flag. `Store::ensure_source` leaves `enabled` untouched on conflict, so an edit that
/// turns a source back on would otherwise never reach the row a previous run disabled.
///
/// The authority runs the other way too: a row the config does not name was left behind by an
/// earlier run, and polling it would make a replaced `sources` list look ignored. The Google
/// seed is exempt — it is not configurable, and `poll_once` never loops over it, so disabling it
/// would only break the one-shot week seed.
fn register_sources(store: &mut Store, config: &Config) -> Result<(), StoreError> {
    let mut configured = Vec::new();
    for (spec, enabled) in config.source_specs() {
        let id = store.ensure_source(&spec, enabled)?;
        store.set_enabled(id, enabled)?;
        configured.push(spec.locator);
    }
    for row in store.sources(false)? {
        let configurable = matches!(row.kind, SourceKind::Rss | SourceKind::Telegram);
        if configurable && !configured.contains(&row.locator) {
            store.set_enabled(row.id, false)?;
        }
    }
    Ok(())
}

/// Delete the database and the write-ahead log sidecars SQLite keeps beside it. Every path is
/// removed unconditionally: a sidecar left behind by an earlier run is recovered against the
/// freshly created database, which is the corruption `--reset-db` exists to prevent. A path
/// that is already absent is success, so a reset on a pristine install still works.
fn reset_database(db_path: &Path) -> std::io::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let path = sidecar_path(db_path, suffix);
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

/// SQLite names its sidecars `<db>-wal` and `<db>-shm`, by appending rather than by replacing
/// the extension: `with_extension` would build the wrong path for any database not named `.sqlite`.
fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// Resolve the configuration, distinguishing the two reasons a path can be missing. A file the
/// user named with `--config` is loaded whatever it says, so a typo is an error rather than a
/// silent run against the built-in source list. Only the platform default may fall back to
/// [`Config::default`], which is the "no config file written yet" case.
fn resolve_config(
    explicit: Option<&Path>,
    default_path: Option<&Path>,
) -> Result<Config, ConfigError> {
    match explicit {
        Some(path) => Config::load(path),
        None => match default_path {
            Some(path) if path.exists() => Config::load(path),
            _ => Ok(Config::default()),
        },
    }
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
    // The log lives in the state directory, which may not exist on a first run. A file logger
    // that cannot be installed is worth saying out loud — the user asked for logging and would
    // otherwise get none — but it is never fatal.
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "Warning: cannot create log directory {}: {error}",
            parent.display()
        );
    }
    let file = match std::fs::File::create(path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("Warning: cannot open log file {}: {error}", path.display());
            return;
        }
    };
    static LOGGER: std::sync::OnceLock<FileLogger> = std::sync::OnceLock::new();
    let logger = LOGGER.get_or_init(|| FileLogger(std::sync::Mutex::new(file)));
    match log::set_logger(logger) {
        Ok(()) => log::set_max_level(parsed),
        Err(error) => eprintln!("Warning: cannot install the file logger: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bakutrend::config::SourceConfig;
    use std::fs;

    #[test]
    fn re_registering_applies_the_config_enabled_flag_in_both_directions() {
        let mut store = Store::open_in_memory().unwrap();
        let source = |enabled: bool| SourceConfig {
            name: "APATV Telegram".to_string(),
            kind: "telegram".to_string(),
            locator: "@apatv".to_string(),
            outlet: "APATV".to_string(),
            enabled,
        };
        let mut config = Config::default();
        config.sources = vec![source(false)];

        register_sources(&mut store, &config).unwrap();
        assert!(
            store.sources(true).unwrap().is_empty(),
            "a disabled source is not polled"
        );

        config.sources = vec![source(true)];
        register_sources(&mut store, &config).unwrap();

        let polled = store.sources(true).unwrap();
        assert_eq!(polled.len(), 1, "the config turns the source back on");
        assert_eq!(polled[0].locator, "@apatv");
    }

    /// A database initialised from the defaults keeps every row it had. A config that names
    /// only one source must therefore switch the others off, or replacing the list changes
    /// nothing for a user who already ran the program once. The Google seed is not in the
    /// configurable list and is not polled per cycle, so it stays on.
    #[test]
    fn a_config_that_omits_a_source_disables_that_row() {
        let mut store = Store::open_in_memory().unwrap();
        let channel = |outlet: &str, locator: &str| SourceSpec {
            kind: SourceKind::Telegram,
            outlet: outlet.to_string(),
            name: format!("{outlet} Telegram"),
            locator: locator.to_string(),
        };
        store.ensure_source(&channel("APA", "@apa_az"), true).unwrap();
        store.ensure_source(&channel("Day.az", "@dayaz"), true).unwrap();
        store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Google,
                    outlet: "Google News".to_string(),
                    name: "Google News 7d".to_string(),
                    locator: "google:7d".to_string(),
                },
                true,
            )
            .unwrap();

        let mut config = Config::default();
        config.sources = vec![SourceConfig {
            name: "APA Telegram".to_string(),
            kind: "telegram".to_string(),
            locator: "@apa_az".to_string(),
            outlet: "APA".to_string(),
            enabled: true,
        }];

        register_sources(&mut store, &config).unwrap();

        let polled: Vec<String> = store
            .sources(true)
            .unwrap()
            .iter()
            .map(|row| row.locator.clone())
            .collect();
        assert_eq!(
            polled,
            ["@apa_az", "google:7d"],
            "config decides which rows are polled; the google seed is exempt"
        );
    }

    #[test]
    fn reset_database_removes_the_database_and_both_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("bakutrend.sqlite");
        let paths = ["", "-wal", "-shm"].map(|suffix| sidecar_path(&db_path, suffix));
        for path in &paths {
            fs::write(path, b"stale").unwrap();
        }

        reset_database(&db_path).unwrap();

        for path in &paths {
            assert!(!path.exists(), "{} survived the reset", path.display());
        }
    }

    #[test]
    fn reset_database_removes_a_stale_wal_without_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("bakutrend.sqlite");
        let wal = sidecar_path(&db_path, "-wal");
        fs::write(&wal, b"stale").unwrap();

        reset_database(&db_path).unwrap();

        assert!(
            !wal.exists(),
            "a stale wal would be recovered against the next database"
        );
    }

    #[test]
    fn reset_database_succeeds_when_nothing_exists() {
        let dir = tempfile::tempdir().unwrap();

        reset_database(&dir.path().join("bakutrend.sqlite")).unwrap();
    }

    #[test]
    fn an_explicit_config_path_that_does_not_exist_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("bakutrend.toml");

        let result = resolve_config(Some(&missing), None);

        assert!(
            result.is_err(),
            "a named config must not silently fall back to the built-in list"
        );
    }

    #[test]
    fn an_explicit_config_path_is_loaded_rather_than_defaulted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mine.toml");
        fs::write(&path, "poll_interval_secs = 42\n").unwrap();

        let config = resolve_config(Some(&path), None).unwrap();

        assert_eq!(config.poll_interval_secs, 42);
    }

    #[test]
    fn a_missing_default_config_path_yields_the_built_in_config() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("config.toml");

        let config = resolve_config(None, Some(&absent)).unwrap();

        assert_eq!(config.sources.len(), Config::default().sources.len());
    }

    #[test]
    fn an_existing_default_config_path_is_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "poll_interval_secs = 77\n").unwrap();

        let config = resolve_config(None, Some(&path)).unwrap();

        assert_eq!(config.poll_interval_secs, 77);
    }
}
