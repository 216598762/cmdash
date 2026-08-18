use std::{fs, path::PathBuf, time::SystemTime};

use crate::config::{AppConfig, ConfigFileError, LoadedConfig};

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ReloadError {
    #[error("could not inspect config {}: {message}", .path.display())]
    Metadata { path: PathBuf, message: String },
    #[error("{0}")]
    Config(ConfigFileError),
}

pub struct ConfigReloader {
    path: PathBuf,
    last_modified: Option<SystemTime>,
}

impl ConfigReloader {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, ReloadError> {
        let path = path.into();
        let last_modified = modified(&path)?;
        Ok(Self {
            path,
            last_modified: Some(last_modified),
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn poll(&mut self) -> Result<Option<AppConfig>, ReloadError> {
        self.poll_with_migrations()
            .map(|loaded| loaded.map(|loaded| loaded.config))
    }

    pub fn poll_with_migrations(&mut self) -> Result<Option<LoadedConfig>, ReloadError> {
        let modified = modified(&self.path)?;
        if self.last_modified.is_some_and(|last| modified <= last) {
            return Ok(None);
        }
        self.last_modified = Some(modified);
        self.load_with_migrations()
            .map(Some)
            .map_err(ReloadError::Config)
    }

    pub fn reload(&mut self) -> Result<AppConfig, ReloadError> {
        self.reload_with_migrations().map(|loaded| loaded.config)
    }

    pub fn reload_with_migrations(&mut self) -> Result<LoadedConfig, ReloadError> {
        self.last_modified = Some(modified(&self.path)?);
        self.load_with_migrations().map_err(ReloadError::Config)
    }

    fn load_with_migrations(&self) -> Result<LoadedConfig, ConfigFileError> {
        AppConfig::load_file_with_migrations(&self.path)
    }
}

fn modified(path: &PathBuf) -> Result<SystemTime, ReloadError> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| ReloadError::Metadata {
            path: path.clone(),
            message: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    #[test]
    fn reload_only_returns_validated_config_changes() {
        let path = std::env::temp_dir().join(format!(
            "cmdash-reload-{}-{}.toml",
            std::process::id(),
            thread::current().name().unwrap_or("test")
        ));
        fs::write(&path, "version = 1\n").unwrap();
        let mut reloader = ConfigReloader::new(path.clone()).unwrap();
        assert!(reloader.poll().unwrap().is_none());

        thread::sleep(Duration::from_millis(5));
        fs::write(&path, "version = 1\n[workspace]\nname = \"reloaded\"\n").unwrap();
        let config = reloader.poll().unwrap().unwrap();
        assert_eq!(config.workspace.name, "reloaded");

        thread::sleep(Duration::from_millis(5));
        fs::write(&path, "version = 2\n").unwrap();
        assert!(matches!(reloader.poll(), Err(ReloadError::Config(_))));
        fs::remove_file(path).unwrap();
    }
}
