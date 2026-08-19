use std::{fs, path::PathBuf, time::SystemTime};

use crate::config::{AppConfig, ConfigFileError, LoadedConfig};

#[cfg(feature = "watch")]
use notify::Watcher;
#[cfg(feature = "watch")]
use std::path::Path;

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

/// An event-driven config watcher for reload-on-save. Only compiled with the
/// `watch` feature; the default build keeps the metadata-polled reloader.
///
/// The watcher watches the config file's parent directory (so an editor's
/// atomic rename-over rewrite is caught) and invokes `on_change` when the
/// config file itself is created, modified, or removed. It deliberately does
/// not reload anything itself: the caller routes the signal through the same
/// re-validate-and-swap reload path as `Ctrl+R`, so a broken mid-save write
/// can never replace a valid runtime.
#[cfg(feature = "watch")]
pub struct ConfigWatcher {
    _watcher: notify::RecommendedWatcher,
}

#[cfg(feature = "watch")]
impl ConfigWatcher {
    /// Starts watching `path` and calling `on_change` on relevant events.
    /// Dropping the returned watcher stops the underlying notify thread.
    pub fn spawn(
        path: impl Into<PathBuf>,
        mut on_change: impl FnMut(notify::Result<notify::Event>) + Send + 'static,
    ) -> Result<Self, ReloadError> {
        let path = path.into();
        let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let watch_dir = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let watched_file = canonical.clone();
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                let Ok(event) = result else {
                    return;
                };
                let touches_config = event.paths.iter().any(|p| p == &watched_file);
                let mutates_config = matches!(
                    event.kind,
                    notify::EventKind::Create(_)
                        | notify::EventKind::Modify(_)
                        | notify::EventKind::Remove(_)
                );
                if touches_config && mutates_config {
                    on_change(Ok(event));
                }
            })
            .map_err(|error| ReloadError::Metadata {
                path: canonical.clone(),
                message: error.to_string(),
            })?;
        watcher
            .watch(&watch_dir, notify::RecursiveMode::NonRecursive)
            .map_err(|error| ReloadError::Metadata {
                path: canonical,
                message: error.to_string(),
            })?;
        Ok(Self { _watcher: watcher })
    }
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

    #[cfg(feature = "watch")]
    #[test]
    fn config_watcher_reports_direct_and_atomic_saves() {
        use std::sync::mpsc;

        let path = std::env::temp_dir().join(format!(
            "cmdash-watch-{}-{}.toml",
            std::process::id(),
            thread::current().name().unwrap_or("watch")
        ));
        fs::write(&path, "version = 1\n").unwrap();

        let (tx, rx) = mpsc::channel();
        let _watcher = ConfigWatcher::spawn(path.clone(), move |result| {
            if let Ok(event) = result {
                let _ = tx.send(event);
            }
        })
        .expect("config watcher should start");
        // Let the notify backend subscribe before mutating the file.
        thread::sleep(Duration::from_millis(50));

        // A plain rewrite is the simplest save path.
        fs::write(&path, "version = 1\n[workspace]\nname = \"direct\"\n").unwrap();
        let direct = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watcher did not report a direct save");

        // Editors save atomically: write a sibling temp file, then rename it
        // over the config. The watcher must catch this because it watches the
        // parent directory, not the inode of the original file.
        let tmp =
            std::env::temp_dir().join(format!("cmdash-watch-{}-tmp.toml", std::process::id()));
        fs::write(&tmp, "version = 1\n[workspace]\nname = \"atomic\"\n").unwrap();
        fs::rename(&tmp, &path).unwrap();
        let atomic = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("watcher did not report an atomic rename-over save");

        for event in [direct, atomic] {
            assert!(
                matches!(
                    event.kind,
                    notify::EventKind::Create(_) | notify::EventKind::Modify(_)
                ),
                "unexpected event kind: {:?}",
                event.kind
            );
        }
        fs::remove_file(path).ok();
    }
}
