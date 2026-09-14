//! Composition root: resolve paths, open the store, then run the poller alone, the web
//! server, or the TUI with the poller on its own thread.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bakutrend::app::{Action, App, AppEvent};
use bakutrend::cli::Cli;
use bakutrend::config::Config;
use bakutrend::dirs::AppDirs;
use bakutrend::error::{ConfigError, StoreError};
use bakutrend::i18n::Lang;
use bakutrend::poller::{Backoff, poll_and_record};
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
        if std::io::stdin().is_terminal() {
            println!(
                "--reset-db will delete {} and its -wal/-shm sidecars.",
                db_path.display()
            );
            print!("Type y to continue: ");
            use std::io::Write as _;
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !confirm_reset(&answer) {
                return Err("aborted: the database was not deleted".into());
            }
        }
        reset_database(&db_path)?;
    }

    let mut config = resolve_config(cli.config.as_deref(), default_config_path.as_deref())?;
    // The flag wins over the file. Both are validated: `Config::load` refuses a language it
    // does not know, and clap refuses one for `--lang`, so this assignment cannot smuggle in a
    // value the screen would then have to guess about.
    if let Some(lang) = &cli.lang {
        config.language.clone_from(lang);
    }
    // Resolved once and copied into the threads that write user-visible text: a status line is
    // part of the screen, and a Russian screen with an English failure in it is half a
    // translation.
    let lang = Lang::parse(&config.language).unwrap_or(Lang::En);

    if let (Some(level), Some(dirs)) = (cli.log.as_deref(), dirs.as_ref()) {
        init_file_logger(level, &dirs.log_path());
    }

    let mut store = Store::open(&db_path)?;
    register_sources(&mut store, &config)?;

    // The Google source is a one-shot week seed, not a configured source; it has its own
    // registration path because it is deliberately absent from the configurable list.
    ensure_google_seed(&mut store)?;

    let now = chrono::Utc::now().timestamp();

    // The web server needs the poller to keep ranking; the poller keeps its write
    // connection on its own thread, exactly as the TUI does.
    if cli.serve || cli.bind.is_some() {
        let bind = cli.bind.unwrap_or_else(|| "127.0.0.1:8080".to_string());
        return run_web(store, config, &db_path, &bind);
    }
    if let Some(dir) = &cli.export {
        return run_export(store, config, &db_path, dir);
    }
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
        // The read loop ended: the TUI would go deaf with no explanation (I12).
        let _ = input_tx.send(AppEvent::InputFailed(
            lang.strings().keyboard_failed.to_string(),
        ));
    });

    let interval = Duration::from_secs(config.poll_interval_secs.max(30));
    let poll_tx = tx.clone();
    // The thread needs its own copy: `config` is still used below to build the UI, and a
    // `move` closure would take it.
    let poll_config = config.clone();
    thread::spawn(move || {
        let mut store = store;
        let mut backoff = Backoff::new();
        loop {
            let now = chrono::Utc::now().timestamp();
            match poll_and_record(&mut store, &poll_config, &fetcher, &mut backoff, now) {
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

    // Spec §6.2: the UI reads on a read-only connection, so it cannot contend with the
    // poller's writer for the WAL lock.
    let reader = Store::open_read_only(&db_path)?;
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
/// earlier run, and polling it would make a replaced `sources` list look ignored. Rows are
/// matched on `(kind, locator)`, the key `sources` is unique on — a config that reuses a
/// locator string under another kind names a different row. The Google seed is exempt: it is
/// not configurable, and `poll_once` never loops over it, so disabling it would only break the
/// one-shot week seed.
fn register_sources(store: &mut Store, config: &Config) -> Result<(), StoreError> {
    let mut configured = Vec::new();
    for (spec, enabled) in config.source_specs() {
        let id = store.ensure_source(&spec, enabled)?;
        store.set_enabled(id, enabled)?;
        configured.push((spec.kind, spec.locator));
    }
    for row in store.sources(false)? {
        let configurable = matches!(row.kind, SourceKind::Rss | SourceKind::Telegram);
        if configurable && !configured.contains(&(row.kind, row.locator)) {
            store.set_enabled(row.id, false)?;
        }
    }
    Ok(())
}

/// Register the Google seed row, which is absent from the configurable source list by
/// design: it exists only to fill the week window once, and `poll_once` keeps it out of the
/// per-cycle loop. `Store::ensure_source` leaves `enabled` alone on conflict, and
/// `register_sources` skips Google rows, so nothing else would ever turn this row back on —
/// the seeder reads it from `Store::sources(true)`, so a row disabled by a hand edit would
/// leave the 7d window empty with nothing to explain why.
fn ensure_google_seed(store: &mut Store) -> Result<i64, StoreError> {
    let id = store.ensure_source(
        &SourceSpec {
            kind: SourceKind::Google,
            outlet: "Google News".to_string(),
            name: "Google News 7d".to_string(),
            locator: "google:7d".to_string(),
        },
        true,
    )?;
    store.set_enabled(id, true)?;
    Ok(id)
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

/// Spec §12: an interactive `--reset-db` destroys accumulated history, so it needs an
/// explicit `y`/`yes`. Extracted so the decision is testable without a terminal.
fn confirm_reset(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
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
                Action::OpenUrl(url) => open_in_browser(app, &url),
                Action::ForcePoll => {
                    let _ = nudge.send(());
                }
                Action::None => {}
            },
            Err(mpsc::RecvTimeoutError::Timeout) => app.tick(chrono::Utc::now().timestamp()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

/// Web server + poller thread. The server holds a read-only view; the poller keeps the
/// ranking snapshots fresh exactly as it does for the TUI, and hands each cycle's report to the
/// page, which shows the same health the TUI's header does.
fn run_web(
    store: Store,
    config: Config,
    db_path: &Path,
    bind: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let fetcher = HttpFetcher::new("azerbaycan OR baki OR bakı")?;
    let poll_config = config.clone();
    let last_poll: bakutrend::web::LastPoll = Default::default();
    let reported = Arc::clone(&last_poll);
    thread::spawn(move || {
        let mut store = store;
        let mut backoff = Backoff::new();
        let interval = Duration::from_secs(poll_config.poll_interval_secs.max(30));
        loop {
            let now = chrono::Utc::now().timestamp();
            match poll_and_record(&mut store, &poll_config, &fetcher, &mut backoff, now) {
                Ok(report) => {
                    log::info!(
                        "poll ok={} new={} samples={} pruned={} failed={}",
                        report.ok,
                        report.new_items,
                        report.samples,
                        report.pruned,
                        report.failed.len()
                    );
                    *reported.lock().expect("poll report mutex poisoned") = Some(report);
                }
                Err(error) => log::error!("poll failed: {error}"),
            }
            thread::sleep(interval);
        }
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(bakutrend::web::serve(db_path, config, bind, last_poll))?;
    Ok(())
}

/// One poll, then the site written from the ranking it produced.
///
/// The snapshot is one reading of the database, so the poll happens here rather than in a loop
/// beside it: a page whose health line says `polled 4m ago` should mean this run polled four
/// minutes before it wrote the page.
fn run_export(
    mut store: Store,
    config: Config,
    db_path: &Path,
    dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let fetcher = HttpFetcher::new("azerbaycan OR baki OR bakı")?;
    let mut backoff = Backoff::new();
    let now = chrono::Utc::now().timestamp();
    let report = match poll_and_record(&mut store, &config, &fetcher, &mut backoff, now) {
        Ok(report) => {
            eprintln!(
                "poll ok={} new={} samples={} pruned={} failed={}",
                report.ok,
                report.new_items,
                report.samples,
                report.pruned,
                report.failed.len()
            );
            Some(report)
        }
        // A poll that failed is not a reason to publish nothing: the store still holds the polls
        // that worked, and the page carries the failure on its own line.
        Err(error) => {
            eprintln!("poll failed: {error}");
            None
        }
    };
    let written = bakutrend::export::write_site(db_path, &config, report.as_ref(), dir)?;
    println!(
        "wrote {} pages, {} files, {} KiB to {}",
        written.pages,
        written.files,
        written.bytes / 1024,
        written.dir.display()
    );
    Ok(())
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
        match poll_and_record(&mut store, &config, &fetcher, &mut backoff, now) {
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

/// Open a URL the user selected. Feed and Telegram content is untrusted input, so only
/// `http://` and `https://` reach the launcher; anything else is refused and named in
/// the status line (I8). A spawn failure is surfaced the same way.
fn open_in_browser(app: &mut App, url: &str) {
    let text = app.lang.strings();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        app.status = format!("{}: {url}", text.non_http_url);
        return;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(error) = std::process::Command::new(opener).arg(url).spawn() {
        app.status = format!("{}: {error}", text.browser_failed);
    }
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
        let mut config = Config {
            sources: vec![source(false)],
            ..Default::default()
        };

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
        store
            .ensure_source(&channel("APA", "@apa_az"), true)
            .unwrap();
        store
            .ensure_source(&channel("Day.az", "@dayaz"), true)
            .unwrap();
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

        let config = Config {
            sources: vec![SourceConfig {
                name: "APA Telegram".to_string(),
                kind: "telegram".to_string(),
                locator: "@apa_az".to_string(),
                outlet: "APA".to_string(),
                enabled: true,
            }],
            ..Default::default()
        };

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

    /// `sources` is unique on `(kind, locator)`, so a config that reuses a locator string under
    /// another kind names a different row. Matching on the locator alone would leave that row
    /// enabled and still polled.
    #[test]
    fn a_locator_the_config_names_under_another_kind_is_disabled() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .ensure_source(
                &SourceSpec {
                    kind: SourceKind::Telegram,
                    outlet: "APA".to_string(),
                    name: "APA Telegram".to_string(),
                    locator: "https://apa.az/rss".to_string(),
                },
                true,
            )
            .unwrap();

        let config = Config {
            sources: vec![SourceConfig {
                name: "APA RSS".to_string(),
                kind: "rss".to_string(),
                locator: "https://apa.az/rss".to_string(),
                outlet: "APA".to_string(),
                enabled: true,
            }],
            ..Default::default()
        };

        register_sources(&mut store, &config).unwrap();

        let rows = store.sources(false).unwrap();
        let telegram = rows
            .iter()
            .find(|row| row.kind == SourceKind::Telegram)
            .expect("the telegram row survives");
        assert!(
            !telegram.enabled,
            "the config named this locator as rss, so the telegram row is not polled"
        );
        assert!(
            rows.iter()
                .any(|row| row.kind == SourceKind::Rss && row.enabled),
            "the rss row the config names is polled"
        );
    }

    /// The seeder reads the Google row from `Store::sources(true)`, and `register_sources`
    /// leaves Google alone on purpose. A row left disabled by an earlier run would otherwise
    /// never seed the week window again.
    #[test]
    fn a_disabled_google_seed_row_is_turned_back_on() {
        let mut store = Store::open_in_memory().unwrap();
        let seed = |store: &mut Store, enabled: bool| {
            store
                .ensure_source(
                    &SourceSpec {
                        kind: SourceKind::Google,
                        outlet: "Google News".to_string(),
                        name: "Google News 7d".to_string(),
                        locator: "google:7d".to_string(),
                    },
                    enabled,
                )
                .unwrap()
        };
        let id = seed(&mut store, false);

        ensure_google_seed(&mut store).unwrap();

        let row = store
            .sources(false)
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .expect("the seed row survives");
        assert!(
            row.enabled,
            "a disabled seed row never runs, and nothing says why"
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
    fn reset_confirmation_accepts_an_explicit_yes_and_refuses_everything_else() {
        for answer in ["y", "Y", "yes", "YES", " yes\n"] {
            assert!(confirm_reset(answer), "{answer:?} must be accepted");
        }
        for answer in ["n", "no", "", "\n", "delete", "y yes"] {
            assert!(!confirm_reset(answer), "{answer:?} must be refused");
        }
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
