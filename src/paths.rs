use anyhow::{Context, Result};
use directories::{BaseDirs, UserDirs};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct MiyuPaths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub secrets_file: PathBuf,
    pub skills_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub pictures_dir: PathBuf,
    pub fish_hook_file: PathBuf,
}

impl MiyuPaths {
    pub fn new() -> Result<Self> {
        let base = BaseDirs::new().context("could not determine XDG base directories")?;
        let config_dir = base.config_dir().join("miyu");
        let data_dir = base.data_dir().join("miyu");
        let cache_dir = base.cache_dir().join("miyu");
        let state_dir = base
            .state_dir()
            .unwrap_or_else(|| base.data_dir())
            .join("miyu");
        let pictures_dir = std::env::var_os("XDG_PICTURES_DIR")
            .map(PathBuf::from)
            .or_else(|| UserDirs::new().and_then(|dirs| dirs.picture_dir().map(PathBuf::from)))
            .unwrap_or_else(|| base.home_dir().join("Pictures"))
            .join("miyu");
        let fish_hook_file = base.config_dir().join("fish/conf.d/miyu.fish");

        Ok(Self {
            config_file: config_dir.join("config.jsonc"),
            secrets_file: config_dir.join("secrets.jsonc"),
            skills_dir: config_dir.join("skills"),
            config_dir,
            data_dir,
            cache_dir,
            state_dir,
            pictures_dir,
            fish_hook_file,
        })
    }

    pub fn create_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.skills_dir)?;
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.cache_dir)?;
        std::fs::create_dir_all(&self.state_dir)?;
        std::fs::create_dir_all(&self.pictures_dir)?;
        Ok(())
    }

    pub fn print(&self) {
        println!("config_dir: {}", self.config_dir.display());
        println!("config_file: {}", self.config_file.display());
        println!("secrets_file: {}", self.secrets_file.display());
        println!("skills_dir: {}", self.skills_dir.display());
        println!("data_dir: {}", self.data_dir.display());
        println!("cache_dir: {}", self.cache_dir.display());
        println!("state_dir: {}", self.state_dir.display());
        println!("pictures_dir: {}", self.pictures_dir.display());
        println!("fish_hook_file: {}", self.fish_hook_file.display());
    }
}
