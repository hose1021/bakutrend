//! Path resolution, following the sibling `ttymap` convention: the `directories` crate
//! v6 with a brand string, and `state_dir()` falling back to `data_local_dir()` because
//! `state_dir()` is Linux-only.

use std::path::PathBuf;

use directories::ProjectDirs;

#[derive(Debug, Clone)]
pub struct AppDirs {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub state: PathBuf,
}

impl AppDirs {
    pub fn resolve() -> Option<Self> {
        let dirs = ProjectDirs::from("", "", "bakutrend")?;
        let state = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .to_path_buf();
        Some(Self {
            config: dirs.config_dir().to_path_buf(),
            data: dirs.data_dir().to_path_buf(),
            cache: dirs.cache_dir().to_path_buf(),
            state,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.data.join("bakutrend.sqlite")
    }

    pub fn config_path(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    pub fn log_path(&self) -> PathBuf {
        self.state.join("bakutrend.log")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_paths_are_branded_and_distinct() {
        let Some(dirs) = AppDirs::resolve() else { return };
        assert!(dirs.db_path().to_string_lossy().contains("bakutrend"));
        assert!(dirs.db_path().ends_with("bakutrend.sqlite"));
        assert!(dirs.config_path().to_string_lossy().ends_with("config.toml"));
        assert!(dirs.log_path().to_string_lossy().contains("bakutrend"));
    }
}
