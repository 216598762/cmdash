//! System clipboard abstraction for cmdash.
//!
//! This module isolates cmdash from the concrete system clipboard
//! implementation so that the copy-mode and OSC 52 paths can share a
//! single backend and tests can inject a mock instead of touching the
//! real clipboard.

/// Clipboard abstraction used by the copy-mode and OSC 52 paths.
///
/// The trait isolates cmdash from the concrete system clipboard
/// implementation so that tests can inject a mock backend instead of
/// touching the real clipboard. The binary holds a `Box<dyn Clipboard>`
/// and uses it directly for reads (OSC 52 queries) and through
/// [`copy_text_to_clipboard`] for writes (copy-mode selections and
/// OSC 52 sets).
///
/// Implementors must be `Send` because the clipboard backend is stored
/// in the tick context, which is moved between threads when the
/// terminal backend is set up.
pub trait Clipboard: Send {
    /// Read plain text from the clipboard.
    ///
    /// Returns the current clipboard contents as a UTF-8 string. On
    /// failure (for example, no clipboard provider is available on a
    /// headless host), returns an error that cmdash logs as a warning.
    fn get_text(&mut self) -> Result<String, Box<dyn std::error::Error + Send + Sync>>;
    /// Write plain text to the clipboard.
    ///
    /// Replaces the current clipboard contents with `text`. On failure,
    /// returns an error that cmdash logs as a warning.
    fn set_text(&mut self, text: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Production clipboard implementation backed by [`arboard`].
///
/// Each operation opens a fresh [`arboard::Clipboard`] handle. This
/// keeps the implementation simple and avoids holding a clipboard
/// handle across the lifetime of the tick context, which can be
/// problematic on some platforms if the clipboard is locked or the
/// display connection changes.
pub struct ArboardClipboard;

impl Clipboard for ArboardClipboard {
    fn get_text(&mut self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let mut clipboard = arboard::Clipboard::new()?;
        Ok(clipboard.get_text()?)
    }

    fn set_text(&mut self, text: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut clipboard = arboard::Clipboard::new()?;
        Ok(clipboard.set_text(text)?)
    }
}

/// Copy the given text to the system clipboard using the provided
/// [`Clipboard`] backend.
///
/// This helper lets the copy-mode path and OSC 52 path share the same
/// clipboard backend, which is replaceable in tests to avoid touching
/// the real system clipboard.
///
/// # Arguments
///
/// * `clipboard` - A mutable reference to the clipboard backend to use.
/// * `text` - The text to copy. Accepts anything that converts into a
///   `String`.
///
/// # Errors
///
/// Returns an error if the underlying clipboard backend fails to write
/// the text (for example, when no clipboard provider is available).
pub fn copy_text_to_clipboard(
    clipboard: &mut dyn Clipboard,
    text: impl Into<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    clipboard.set_text(&text.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock clipboard backend that records operations for test assertions.
    struct MockClipboard {
        stored: Option<String>,
        /// Number of times set_text was called.
        set_count: usize,
        /// Number of times get_text was called.
        get_count: usize,
        /// If true, all operations return an error.
        fail: bool,
    }

    impl MockClipboard {
        fn new() -> Self {
            Self {
                stored: None,
                set_count: 0,
                get_count: 0,
                fail: false,
            }
        }

        fn with_content(text: &str) -> Self {
            Self {
                stored: Some(text.to_string()),
                set_count: 0,
                get_count: 0,
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                stored: None,
                set_count: 0,
                get_count: 0,
                fail: true,
            }
        }
    }

    impl Clipboard for MockClipboard {
        fn get_text(&mut self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            self.get_count += 1;
            if self.fail {
                return Err("mock clipboard failure".into());
            }
            Ok(self.stored.clone().unwrap_or_default())
        }

        fn set_text(&mut self, text: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.set_count += 1;
            if self.fail {
                return Err("mock clipboard failure".into());
            }
            self.stored = Some(text.to_string());
            Ok(())
        }
    }

    #[test]
    fn mock_clipboard_set_and_get() {
        let mut clip = MockClipboard::new();
        clip.set_text("hello world").unwrap();
        assert_eq!(clip.get_text().unwrap(), "hello world");
    }

    #[test]
    fn mock_clipboard_with_content_initializes_correctly() {
        let mut clip = MockClipboard::with_content("initial");
        assert_eq!(clip.get_text().unwrap(), "initial");
    }

    #[test]
    fn mock_clipboard_empty_by_default() {
        let mut clip = MockClipboard::new();
        assert_eq!(clip.get_text().unwrap(), "");
    }

    #[test]
    fn mock_clipboard_overwrites_on_set() {
        let mut clip = MockClipboard::with_content("old");
        clip.set_text("new").unwrap();
        assert_eq!(clip.get_text().unwrap(), "new");
    }

    #[test]
    fn mock_clipboard_records_operation_counts() {
        let mut clip = MockClipboard::new();
        clip.set_text("a").unwrap();
        clip.set_text("b").unwrap();
        let _ = clip.get_text();
        assert_eq!(clip.set_count, 2);
        assert_eq!(clip.get_count, 1);
    }

    #[test]
    fn copy_text_to_clipboard_delegates_to_set_text() {
        let mut clip = MockClipboard::new();
        copy_text_to_clipboard(&mut clip, "copied").unwrap();
        assert_eq!(clip.get_text().unwrap(), "copied");
        assert_eq!(clip.set_count, 1);
    }

    #[test]
    fn copy_text_to_clipboard_accepts_string() {
        let mut clip = MockClipboard::new();
        copy_text_to_clipboard(&mut clip, String::from("owned")).unwrap();
        assert_eq!(clip.get_text().unwrap(), "owned");
    }

    #[test]
    fn copy_text_to_clipboard_propagates_errors() {
        let mut clip = MockClipboard::failing();
        let result = copy_text_to_clipboard(&mut clip, "fail");
        assert!(result.is_err());
    }

    #[test]
    fn mock_clipboard_get_propagates_errors() {
        let mut clip = MockClipboard::failing();
        let result = clip.get_text();
        assert!(result.is_err());
    }

    #[test]
    fn mock_clipboard_set_propagates_errors() {
        let mut clip = MockClipboard::failing();
        let result = clip.set_text("x");
        assert!(result.is_err());
    }

    #[test]
    fn copy_text_to_clipboard_with_empty_string() {
        let mut clip = MockClipboard::with_content("old");
        copy_text_to_clipboard(&mut clip, "").unwrap();
        assert_eq!(clip.get_text().unwrap(), "");
    }

    #[test]
    fn copy_text_to_clipboard_with_unicode() {
        let mut clip = MockClipboard::new();
        copy_text_to_clipboard(&mut clip, "こんにちは 🎉").unwrap();
        assert_eq!(clip.get_text().unwrap(), "こんにちは 🎉");
    }
}
