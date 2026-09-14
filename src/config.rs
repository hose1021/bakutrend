//! TOML configuration. A missing default path is not an error; an explicit path that
//! does not exist is.

use std::path::Path;

use serde::Deserialize;

use crate::error::ConfigError;
use crate::i18n::Lang;
use crate::score::Weights;
use crate::source::{SourceKind, SourceSpec};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Retention {
    pub view_sample_days: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            view_sample_days: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    pub name: String,
    pub kind: String,
    pub locator: String,
    pub outlet: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub poll_interval_secs: u64,
    pub cluster_threshold: f64,
    /// Cosine similarity at which two items are the same story without sharing a single
    /// word. Only consulted when embeddings exist; with no provider configured the lexical
    /// rule decides alone and this value changes nothing.
    pub semantic_threshold: f64,
    pub weights: Weights,
    pub retention: Retention,
    /// Interface language: `en`, `az` or `ru`. The news itself is never translated.
    pub language: String,
    pub sources: Vec<SourceConfig>,
}

/// The 21 configured sources: 11 RSS feeds and 10 Telegram channels.
/// `@apatv` answered during design research but now returns a preview-less stub, so it
/// ships disabled (2026-09-14); flip it on when it serves posts again.
fn default_sources() -> Vec<SourceConfig> {
    let rss = [
        ("Qafqazinfo RSS", "https://qafqazinfo.az/rss", "Qafqazinfo"),
        ("APA RSS", "https://apa.az/rss", "APA"),
        ("Azertag RSS", "https://azertag.az/rss", "Azertag"),
        ("Report RSS", "https://report.az/rss/", "Report"),
        ("Modern.az RSS", "https://modern.az/rss", "Modern.az"),
        ("Olke.az RSS", "https://olke.az/rss", "Olke.az"),
        ("Oxu.az RSS", "https://oxu.az/feed", "Oxu.az"),
        ("Baku.ws RSS", "https://baku.ws/rss", "Baku.ws"),
        ("Trend.az RSS", "https://trend.az/rss/", "Trend.az"),
        ("Minval RSS", "https://minval.az/rss", "Minval"),
        ("Haqqin.az RSS", "https://haqqin.az/rss.xml", "Haqqin.az"),
    ];
    let telegram = [
        ("Qafqazinfo Telegram", "@qafqazinfo", "Qafqazinfo", true),
        ("APA Telegram", "@apa_az", "APA", true),
        ("Day.az Telegram", "@dayaz", "Day.az", true),
        ("Axar.az Telegram", "@axaraz", "Axar.az", true),
        ("Minval Telegram", "@minval_az", "Minval", true),
        ("Report Telegram", "@reportnewsaz", "Report", true),
        ("APATV Telegram", "@apatv", "Apa TV", false),
        ("Baku Post Telegram", "@bakupost", "Baku Post", true),
        ("Qaynarinfo Telegram", "@qaynarinfo", "Qaynarinfo", true),
        ("Meydan TV Telegram", "@meydantv", "Meydan TV", true),
    ];

    let mut sources: Vec<SourceConfig> = rss
        .iter()
        .map(|(name, locator, outlet)| SourceConfig {
            name: name.to_string(),
            kind: "rss".to_string(),
            locator: locator.to_string(),
            outlet: outlet.to_string(),
            enabled: true,
        })
        .collect();
    sources.extend(
        telegram
            .iter()
            .map(|(name, locator, outlet, enabled)| SourceConfig {
                name: name.to_string(),
                kind: "telegram".to_string(),
                locator: locator.to_string(),
                outlet: outlet.to_string(),
                enabled: *enabled,
            }),
    );
    sources
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_interval_secs: 300,
            cluster_threshold: 0.40,
            semantic_threshold: 0.80,
            weights: Weights::default(),
            retention: Retention::default(),
            language: "az".to_string(),
            sources: default_sources(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Self = toml::from_str(&raw).map_err(|source| ConfigError::Toml {
            path: path.to_path_buf(),
            source,
        })?;
        // Reject unknown kinds here rather than dropping the source later: a typo would
        // otherwise delete a configured source with nothing to explain it.
        for source in &config.sources {
            if SourceKind::parse(&source.kind).is_none() {
                return Err(ConfigError::UnknownSourceKind {
                    name: source.name.clone(),
                    kind: source.kind.clone(),
                });
            }
        }
        config.validate()?;
        Ok(config)
    }

    /// Reject a number that parses but cannot mean anything, before the database is opened and
    /// the first poll starts.
    ///
    /// Each of these would otherwise surface far from the file that caused it. A NaN threshold
    /// silently stops matching. A negative weight subtracts from the score. A weight sum of
    /// zero is a division by nothing, and a retention of `i64::MAX` days overflows the
    /// multiplication that turns days into seconds — in release that wraps into a negative
    /// cutoff, which deletes every stored sample instead of reporting anything.
    ///
    /// Weights need not sum to one: the score divides by their sum, so any finite set of zero
    /// or more with a positive total is a valid weighting. Only a total that is zero, negative
    /// or infinite is refused.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.poll_interval_secs == 0 {
            return Err(invalid("poll_interval_secs", 0, "at least 1 second"));
        }
        for (field, value) in [
            ("cluster_threshold", self.cluster_threshold),
            ("semantic_threshold", self.semantic_threshold),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err(invalid(field, value, "a finite number from 0 to 1"));
            }
        }
        for (field, value) in [
            ("weights.coverage", self.weights.coverage),
            ("weights.engagement", self.weights.engagement),
            ("weights.freshness", self.weights.freshness),
            ("weights.spread_velocity", self.weights.spread_velocity),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(invalid(field, value, "a finite number of 0 or more"));
            }
        }
        let total = self.weights.coverage
            + self.weights.engagement
            + self.weights.freshness
            + self.weights.spread_velocity;
        if !total.is_finite() || total <= 0.0 {
            return Err(invalid(
                "weights",
                total,
                "a positive, finite sum, so at least one weight is above zero",
            ));
        }
        if self.retention.view_sample_days < 1 {
            return Err(invalid(
                "retention.view_sample_days",
                self.retention.view_sample_days,
                "1 day or more",
            ));
        }
        if Lang::parse(&self.language).is_none() {
            return Err(invalid("language", &self.language, "one of en, az or ru"));
        }
        if self
            .retention
            .view_sample_days
            .checked_mul(86_400)
            .is_none()
        {
            return Err(invalid(
                "retention.view_sample_days",
                self.retention.view_sample_days,
                "a number of days that still fits in seconds",
            ));
        }
        Ok(())
    }

    /// Config entries become source specs. Every kind is already valid: [`Config::load`]
    /// rejects unknown ones, and [`Config::default`] only ships known kinds.
    pub fn source_specs(&self) -> Vec<(SourceSpec, bool)> {
        self.sources
            .iter()
            .filter_map(|source| {
                let kind = SourceKind::parse(&source.kind)?;
                Some((
                    SourceSpec {
                        kind,
                        outlet: source.outlet.clone(),
                        name: source.name.clone(),
                        locator: source.locator.clone(),
                    },
                    source.enabled,
                ))
            })
            .collect()
    }
}

/// One rejected number, with the field and the accepted range in the message.
fn invalid(field: &str, value: impl std::fmt::Display, expected: &str) -> ConfigError {
    ConfigError::InvalidValue {
        field: field.to_string(),
        value: value.to_string(),
        expected: expected.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_ships_twenty_one_sources_with_twenty_enabled() {
        let config = Config::default();
        assert_eq!(config.sources.len(), 21);
        let enabled = config.sources.iter().filter(|s| s.enabled).count();
        assert_eq!(
            enabled, 20,
            "apatv ships disabled until it serves a preview again"
        );
        assert_eq!(
            config
                .sources
                .iter()
                .filter(|s| !s.enabled)
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["APATV Telegram"]
        );
    }

    #[test]
    fn default_config_matches_the_agreed_values() {
        let config = Config::default();
        assert_eq!(config.poll_interval_secs, 300);
        assert!((config.cluster_threshold - 0.40).abs() < 1e-9);
        assert!((config.semantic_threshold - 0.80).abs() < 1e-9);
        assert!((config.weights.coverage - 0.35).abs() < 1e-9);
        assert!((config.weights.spread_velocity - 0.10).abs() < 1e-9);
        assert_eq!(config.retention.view_sample_days, 30);
    }

    #[test]
    fn source_specs_carry_kind_locator_and_outlet() {
        let specs = Config::default().source_specs();
        assert_eq!(specs.len(), 21);
        assert!(specs.iter().any(|(spec, enabled)| {
            spec.kind == SourceKind::Rss
                && spec.locator == "https://oxu.az/feed"
                && spec.name == "Oxu.az RSS"
                && spec.outlet == "Oxu.az"
                && *enabled
        }));
        assert!(specs.iter().any(|(spec, enabled)| {
            spec.kind == SourceKind::Rss && spec.locator == "https://qafqazinfo.az/rss" && *enabled
        }));
        assert!(specs.iter().any(|(spec, _)| {
            spec.kind == SourceKind::Telegram
                && spec.locator == "@qafqazinfo"
                && spec.outlet == "Qafqazinfo"
        }));
    }

    #[test]
    fn a_missing_file_falls_back_to_defaults() {
        let config = Config::load(std::path::Path::new("/nonexistent/bakutrend.toml"));
        assert!(
            config.is_err(),
            "an explicit path that does not exist is an error, not a silent default"
        );
    }

    #[test]
    fn partial_toml_overrides_only_the_keys_it_sets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "poll_interval_secs = 900\n").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.poll_interval_secs, 900);
        assert_eq!(config.sources.len(), 21, "unset keys keep their defaults");
    }

    #[test]
    fn a_partial_weights_table_keeps_the_keys_it_omits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[weights]\ncoverage = 0.5\n").unwrap();

        let config = Config::load(&path).unwrap();
        assert!((config.weights.coverage - 0.5).abs() < 1e-9);
        assert!(
            (config.weights.engagement - 0.35).abs() < 1e-9,
            "an omitted key keeps its default"
        );
        assert!(
            (config.weights.freshness - 0.20).abs() < 1e-9,
            "an omitted key keeps its default"
        );
        assert!(
            (config.weights.spread_velocity - 0.10).abs() < 1e-9,
            "a weights table written before spread_velocity existed still loads"
        );
    }

    #[test]
    fn a_present_but_empty_retention_table_keeps_its_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[retention]\n").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(
            config.retention.view_sample_days, 30,
            "an omitted key keeps its default"
        );
    }

    #[test]
    fn an_unknown_source_kind_fails_the_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let source = |kind: &str| {
            format!(
                "[[sources]]\nname = \"Broken Feed\"\nkind = \"{kind}\"\n\
                 locator = \"https://example.test/rss\"\noutlet = \"Example\"\n"
            )
        };

        std::fs::write(&path, source("rsss")).unwrap();
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("Broken Feed"), "error names the source: {err}");
        assert!(err.contains("rsss"), "error names the bad kind: {err}");

        std::fs::write(&path, source("rss")).unwrap();
        assert!(Config::load(&path).is_ok(), "a valid kind still loads");

        std::fs::write(&path, source("telegram")).unwrap();
        assert!(Config::load(&path).is_ok(), "a valid kind still loads");
    }

    /// Load `body` as a config file and return the error message, if any.
    fn load(body: &str) -> Result<Config, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        Config::load(&path).map_err(|error| error.to_string())
    }

    #[test]
    fn a_threshold_outside_zero_to_one_or_not_finite_fails_the_load() {
        // TOML parses `nan` and `inf`, so both reach the validator and neither may pass: a NaN
        // threshold makes every comparison false and stops matching entirely.
        for (body, field) in [
            ("cluster_threshold = 1.5\n", "cluster_threshold"),
            ("cluster_threshold = -0.1\n", "cluster_threshold"),
            ("cluster_threshold = nan\n", "cluster_threshold"),
            ("semantic_threshold = inf\n", "semantic_threshold"),
            ("semantic_threshold = -inf\n", "semantic_threshold"),
        ] {
            let error = load(body).expect_err(body);
            assert!(error.contains(field), "names the field: {error}");
            assert!(error.contains("0 to 1"), "names the range: {error}");
        }

        assert!(load("cluster_threshold = 0.0\n").is_ok());
        assert!(load("cluster_threshold = 1.0\n").is_ok());
    }

    #[test]
    fn an_invalid_weight_set_fails_the_load() {
        for (body, field) in [
            ("[weights]\ncoverage = -0.1\n", "weights.coverage"),
            ("[weights]\nengagement = nan\n", "weights.engagement"),
            ("[weights]\nfreshness = inf\n", "weights.freshness"),
            (
                "[weights]\nspread_velocity = -1.0\n",
                "weights.spread_velocity",
            ),
        ] {
            let error = load(body).expect_err(body);
            assert!(error.contains(field), "names the field: {error}");
            assert!(error.contains("0 or more"), "names the range: {error}");
        }

        // Every weight at zero leaves nothing to divide by: the score would be 0/0. A partial
        // table cannot reach that, because the keys it omits keep their defaults.
        let error =
            load("[weights]\ncoverage = 0\nengagement = 0\nfreshness = 0\nspread_velocity = 0\n")
                .expect_err("a zero total");
        assert!(error.contains("weights"), "names the field: {error}");
        assert!(error.contains("above zero"), "names the range: {error}");
    }

    #[test]
    fn weights_that_do_not_sum_to_one_still_load() {
        // The score divides by the sum of the weights, so any positive set is a valid
        // weighting. Rejecting these would forbid re-weighting one signal above the rest.
        let config = load(
            "[weights]\ncoverage = 1.0\nengagement = 0.5\nfreshness = 0.25\nspread_velocity = 0.25\n",
        )
        .expect("weights summing to two are valid");
        assert!((config.weights.coverage - 1.0).abs() < 1e-9);
        assert!(config.validate().is_ok());
    }

    /// A language the screen cannot speak is refused at load, naming the field and the values
    /// that would work, rather than rendering an English screen to someone who asked for
    /// Azerbaijani.
    #[test]
    fn an_unknown_language_fails_the_load() {
        for body in [
            "language = \"de\"\n",
            "language = \"english\"\n",
            "language = \"\"\n",
        ] {
            let error = load(body).expect_err(body);
            assert!(error.contains("language"), "names the field: {error}");
            assert!(
                error.contains("en, az or ru"),
                "names the values that work: {error}"
            );
        }
        for code in ["en", "az", "ru", "AZ", "ru_RU.UTF-8"] {
            let body = format!("language = \"{code}\"\n");
            assert!(load(&body).is_ok(), "{code} must load");
        }
    }

    #[test]
    fn an_impossible_retention_or_poll_interval_fails_the_load() {
        for (body, field) in [
            ("[retention]\nview_sample_days = 0\n", "view_sample_days"),
            ("[retention]\nview_sample_days = -5\n", "view_sample_days"),
            (
                "[retention]\nview_sample_days = 9223372036854775807\n",
                "view_sample_days",
            ),
        ] {
            let error = load(body).expect_err(body);
            assert!(error.contains(field), "names the field: {error}");
        }
        // i64::MAX days overflow the days-to-seconds multiplication, so the message has to say
        // which of the two retention rules was broken.
        let error = load("[retention]\nview_sample_days = 9223372036854775807\n").unwrap_err();
        assert!(error.contains("fits in seconds"), "{error}");

        let error = load("poll_interval_secs = 0\n").expect_err("a zero interval");
        assert!(error.contains("poll_interval_secs"), "{error}");
    }
}
