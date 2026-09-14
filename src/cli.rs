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

    /// Configuration file. Defaults to the platform config directory.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Write logs to a file. Takes an optional level, defaulting to `debug`.
    #[arg(long, num_args = 0..=1, default_missing_value = "debug")]
    pub log: Option<String>,

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

    #[test]
    fn log_takes_an_optional_level_defaulting_to_debug() {
        let args = Cli::try_parse_from(["bakutrend", "--log"]).unwrap();
        assert_eq!(args.log.as_deref(), Some("debug"));

        let args = Cli::try_parse_from(["bakutrend", "--log", "info"]).unwrap();
        assert_eq!(args.log.as_deref(), Some("info"));

        let args = Cli::try_parse_from(["bakutrend"]).unwrap();
        assert_eq!(args.log, None);
    }
}
