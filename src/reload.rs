use std::{fmt, fs, path::PathBuf, time::SystemTime};

use crate::config::{AppConfig, ConfigFileError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReloadError {
    Metadata { path: PathBuf, message: String },
    Config(ConfigFileError),
}

impl fmt::Display for ReloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Metadata { path, message } => {
                write!(
                    formatter,
                    "could not inspect config {}: {message}",
                    path.display()
                )
            }
            Self::Config(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ReloadError {}

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
        let modified = modified(&self.path)?;
        if self.last_modified.is_some_and(|last| modified <= last) {
            return Ok(None);
        }
        self.last_modified = Some(modified);
        self.load().map(Some).map_err(ReloadError::Config)
    }

    pub fn reload(&mut self) -> Result<AppConfig, ReloadError> {
        self.last_modified = Some(modified(&self.path)?);
        self.load().map_err(ReloadError::Config)
    }

    fn load(&self) -> Result<AppConfig, ConfigFileError> {
        AppConfig::load_file(&self.path)
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
