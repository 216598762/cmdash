use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrashReport {
    pub message: String,
    pub context: Vec<String>,
}

impl CrashReport {
    pub fn from_error(error: impl ToString) -> Self {
        Self {
            message: error.to_string(),
            context: Vec::new(),
        }
    }

    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        if self.context.len() < 16 {
            self.context.push(context.into());
        }
        self
    }

    pub fn write_to(&self, directory: impl AsRef<Path>) -> io::Result<PathBuf> {
        let directory = directory.as_ref();
        fs::create_dir_all(directory)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let path = directory.join(format!(
            "cmdash-crash-{}-{timestamp}.txt",
            std::process::id()
        ));
        let mut output = String::from("cmdash crash reproduction artifact\n\n");
        output.push_str("message = ");
        output.push_str(&self.message);
        output.push('\n');
        for context in &self.context {
            output.push_str("context = ");
            output.push_str(context);
            output.push('\n');
        }
        fs::write(&path, output)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_bounded_reproduction_artifact() {
        let directory = std::env::temp_dir().join(format!("cmdash-crash-{}", std::process::id()));
        let report = CrashReport::from_error("frame failed").with_context("widget=terminal");
        let path = report.write_to(&directory).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("frame failed"));
        assert!(contents.contains("widget=terminal"));
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }
}
