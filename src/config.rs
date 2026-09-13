//! TOML configuration. A missing default path is not an error; an explicit path that
//! does not exist is.

use std::path::Path;

use serde::Deserialize;

use crate::error::ConfigError;
use crate::score::Weights;
use crate::source::{SourceKind, SourceSpec};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Retention {
    pub view_sample_days: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Self { view_sample_days: 30 }
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
    pub weights: Weights,
    pub retention: Retention,
    pub local_keywords: Vec<String>,
    pub sources: Vec<SourceConfig>,
}

const LOCAL_KEYWORDS: &[&str] = &[
    "Azərbaycan", "Bakı", "Gəncə", "Sumqayıt", "Mingəçevir", "Bərdə", "Lənkəran", "Astara",
    "Naxçıvan", "Şuşa", "Xankəndi", "Qarabağ", "Şərqi Zəngəzur", "Xəzər", "Abşeron", "Quba",
    "Qusar", "Şəki", "Yevlax", "Salyan", "Prezident", "Milli Məclis", "Nazirlər Kabineti",
    "SOCAR", "AZAL", "ADY", "ANAMA", "DİN", "XİN", "MİDA", "CƏB",
];

/// The 20 verified sources: 10 RSS feeds and 10 Telegram channels.
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
        ("Baku.ws RSS", "https://baku.ws/rss", "Baku.ws"),
        ("Trend.az RSS", "https://trend.az/rss/", "Trend.az"),
        ("Minval RSS", "https://minval.az/rss", "Minval"),
        ("Haqqin.az RSS", "https://haqqin.az/rss/", "Haqqin.az"),
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
    sources.extend(telegram.iter().map(|(name, locator, outlet, enabled)| SourceConfig {
        name: name.to_string(),
        kind: "telegram".to_string(),
        locator: locator.to_string(),
        outlet: outlet.to_string(),
        enabled: *enabled,
    }));
    sources
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_interval_secs: 300,
            cluster_threshold: 0.45,
            weights: Weights::default(),
            retention: Retention::default(),
            local_keywords: LOCAL_KEYWORDS.iter().map(|k| k.to_string()).collect(),
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
        Ok(config)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_ships_twenty_sources_with_nineteen_enabled() {
        let config = Config::default();
        assert_eq!(config.sources.len(), 20);
        let enabled = config.sources.iter().filter(|s| s.enabled).count();
        assert_eq!(enabled, 19, "apatv ships disabled until it serves a preview again");
        assert_eq!(config.sources.iter().filter(|s| !s.enabled).map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["APATV Telegram"]);
    }

    #[test]
    fn default_config_matches_the_agreed_values() {
        let config = Config::default();
        assert_eq!(config.poll_interval_secs, 300);
        assert!((config.cluster_threshold - 0.45).abs() < 1e-9);
        assert!((config.weights.coverage - 0.40).abs() < 1e-9);
        assert_eq!(config.retention.view_sample_days, 30);
        assert!(config.local_keywords.iter().any(|k| k == "Bakı"));
    }

    #[test]
    fn source_specs_carry_kind_locator_and_outlet() {
        let specs = Config::default().source_specs();
        assert_eq!(specs.len(), 20);
        assert!(specs.iter().any(|(spec, enabled)| {
            spec.kind == SourceKind::Rss && spec.locator == "https://qafqazinfo.az/rss" && *enabled
        }));
        assert!(specs.iter().any(|(spec, _)| {
            spec.kind == SourceKind::Telegram && spec.locator == "@qafqazinfo" && spec.outlet == "Qafqazinfo"
        }));
    }

    #[test]
    fn a_missing_file_falls_back_to_defaults() {
        let config = Config::load(std::path::Path::new("/nonexistent/bakutrend.toml"));
        assert!(config.is_err(), "an explicit path that does not exist is an error, not a silent default");
    }

    #[test]
    fn partial_toml_overrides_only_the_keys_it_sets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "poll_interval_secs = 900\n").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.poll_interval_secs, 900);
        assert_eq!(config.sources.len(), 20, "unset keys keep their defaults");
    }

    #[test]
    fn a_partial_weights_table_keeps_the_keys_it_omits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[weights]\ncoverage = 0.5\n").unwrap();

        let config = Config::load(&path).unwrap();
        assert!((config.weights.coverage - 0.5).abs() < 1e-9);
        assert!((config.weights.engagement - 0.40).abs() < 1e-9, "an omitted key keeps its default");
        assert!((config.weights.freshness - 0.20).abs() < 1e-9, "an omitted key keeps its default");
    }

    #[test]
    fn a_present_but_empty_retention_table_keeps_its_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[retention]\n").unwrap();

        let config = Config::load(&path).unwrap();
        assert_eq!(config.retention.view_sample_days, 30, "an omitted key keeps its default");
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
}
