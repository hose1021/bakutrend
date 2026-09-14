//! Command-line surface.

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "bakutrend",
    about = "Rank Azerbaijani news by popularity",
    version
)]
pub struct Cli {
    /// Run the poller without the TUI, so week-history keeps accumulating.
    #[arg(long)]
    pub poll_only: bool,

    /// Serve the ranking as a web page on `--bind` instead of opening the TUI.
    #[arg(long)]
    pub serve: bool,

    /// Address the web server listens on. `host:port`; implies `--serve`.
    #[arg(long)]
    pub bind: Option<String>,

    /// Poll once, then write a static snapshot of the web page into `DIR` and exit. A host that
    /// runs nothing serves it; the text filter and the card per story are not part of it.
    #[arg(long, value_name = "DIR")]
    pub export: Option<PathBuf>,

    /// Configuration file. Defaults to the platform config directory.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Write logs to a file. Takes an optional level, defaulting to `debug`.
    #[arg(long, num_args = 0..=1, default_missing_value = "debug")]
    pub log: Option<String>,

    /// Interface language. Overrides `language` from the config file.
    #[arg(long, value_parser = ["en", "az", "ru"])]
    pub lang: Option<String>,

    /// Delete the database and rebuild it from scratch.
    #[arg(long)]
    pub reset_db: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn poll_only_and_reset_db_flags_parse() {
        let args = Cli::try_parse_from(["bakutrend", "--poll-only", "--reset-db"]).unwrap();
        assert!(args.poll_only);
        assert!(args.reset_db);
        assert!(args.config.is_none());
    }

    /// clap refuses a language the screen cannot speak, so a typo cannot silently become the
    /// default English screen.
    #[test]
    fn lang_accepts_the_three_languages_and_refuses_anything_else() {
        for code in ["en", "az", "ru"] {
            let args = Cli::try_parse_from(["bakutrend", "--lang", code]).unwrap();
            assert_eq!(args.lang.as_deref(), Some(code));
        }
        assert!(Cli::try_parse_from(["bakutrend", "--lang", "de"]).is_err());
        assert!(Cli::try_parse_from(["bakutrend"]).unwrap().lang.is_none());
    }

    #[test]
    fn log_takes_an_optional_level_defaulting_to_debug() {
        let args = Cli::try_parse_from(["bakutrend", "--log"]).unwrap();
        assert_eq!(args.log.as_deref(), Some("debug"));

        let args = Cli::try_parse_from(["bakutrend", "--log", "info"]).unwrap();
        assert_eq!(args.log.as_deref(), Some("info"));

        let args = Cli::try_parse_from(["bakutrend"]).unwrap();
        assert_eq!(args.log, None);
    }

    /// `--bind` alone names the server: `main` treats `serve || bind` the same, so the parse
    /// keeps the two flags independent and the composition root decides.
    #[test]
    fn bind_alone_parses_as_a_web_run() {
        let args = Cli::try_parse_from(["bakutrend", "--bind", "127.0.0.1:8080"]).unwrap();
        assert_eq!(args.bind.as_deref(), Some("127.0.0.1:8080"));

        let args = Cli::try_parse_from(["bakutrend", "--serve"]).unwrap();
        assert!(args.serve);
        assert!(args.bind.is_none());

        let args = Cli::try_parse_from(["bakutrend"]).unwrap();
        assert!(!args.serve);
        assert!(args.bind.is_none());
    }

    /// `--export` names the directory it writes, and a run without it opens the TUI.
    #[test]
    fn export_takes_the_directory_it_writes() {
        let args = Cli::try_parse_from(["bakutrend", "--export", "site"]).unwrap();
        assert_eq!(args.export.as_deref(), Some(std::path::Path::new("site")));
        assert!(Cli::try_parse_from(["bakutrend"]).unwrap().export.is_none());
        assert!(Cli::try_parse_from(["bakutrend", "--export"]).is_err());
    }
}
