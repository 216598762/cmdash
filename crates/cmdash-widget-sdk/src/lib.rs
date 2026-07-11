//! c-ABI-safe `CmdashWidget` trait plus a pinned ABI version, for
//! dynamic `.so` / `.dll` widgets loaded via `libloading`.
//!
//! ## Overview
//!
//! A cmdash widget is a dynamically-loaded shared library (`.so` on
//! Linux, `.dll` on Windows, `.dylib` on macOS) that implements the
//! [`CmdashWidget`] trait. The host binary loads widgets at startup
//! via `libloading` and calls [`CmdashWidget::render`] once per frame
//! (phase 3a) for each `pane kind=widget` leaf in the layout tree.
//!
//! ## ABI contract
//!
//! Every widget `.so` MUST export a `cmdash_widget_create` function
//! with C linkage. The [`cmdash_widget_export!`] macro generates this
//! function for you:
//!
//! ```ignore
//! use cmdash_widget_sdk::{cmdash_widget_export, CmdashWidget, WidgetEvent};
//!
//! struct MyWidget { /* ... */ }
//!
//! impl CmdashWidget for MyWidget {
//!     fn render(&mut self, area: ratatui::layout::Rect, frame: &mut ratatui::Frame) {
//!         // draw into frame
//!     }
//!     fn on_event(&mut self, _event: &WidgetEvent) {}
//! }
//!
//! cmdash_widget_export!(MyWidget);
//! ```
//!
//! The host checks [`CMDASH_WIDGET_ABI_VERSION`] at load time; a
//! mismatch logs a warning and skips the widget.
//!
//! ## Version compatibility
//!
//! The widget `.so` and the cmdash host binary MUST use the same
//! `ratatui` major.minor version (the `Frame` type layout must
//! match). Rebuild widgets when upgrading cmdash's ratatui dep.

use std::ffi::c_void;

/// Pinned ABI version for the widget interface. The host checks this
/// at load time against [`CMDASH_WIDGET_ABI_VERSION`] exported by the
/// widget `.so`. A mismatch means the widget was compiled against a
/// different SDK version and must be recompiled.
pub const CMDASH_WIDGET_ABI_VERSION: u32 = 1;

/// Dynamically-loaded widget instance. Implement this trait in your
/// widget `.so` and export it via [`cmdash_widget_export!`].
///
/// ## Object safety
///
/// This trait is object-safe: the host stores widgets as
/// `Box<dyn CmdashWidget>`. All methods take `&mut self` or `&self`
/// with concrete argument types.
pub trait CmdashWidget: Send {
    /// Human-readable widget name for logging and diagnostics.
    fn name(&self) -> &str;

    /// Render the widget into the given `area` of the ratatui `Frame`.
    /// Called once per frame (~30 fps) for each `pane kind=widget`
    /// leaf in the layout tree. The `area` is the cell-grid rect
    /// assigned to this widget pane by the layout engine.
    fn render(&mut self, area: ratatui::layout::Rect, frame: &mut ratatui::Frame);

    /// Handle an input or lifecycle event. Called by the host when
    /// the focused pane is a widget pane and a matching event occurs.
    fn on_event(&mut self, _event: &WidgetEvent) {}
}

/// Events forwarded from the host to a focused widget pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidgetEvent {
    /// A key press event. The character is provided for `Char` keys;
    /// `KeyCode` variants like arrows and F-keys are represented as
    /// their crossterm names.
    Key {
        /// The character typed, if this is a `Char` key event.
        code: KeyCode,
        /// Modifier keys held during the event.
        modifiers: KeyModifiers,
    },
    /// The widget pane was resized. `width` and `height` are the new
    /// cell-grid dimensions.
    Resize { width: u16, height: u16 },
    /// The widget pane gained input focus.
    FocusGained,
    /// The widget pane lost input focus.
    FocusLost,
}

/// Simplified key code for widget events. Mirrors the common subset
/// of `crossterm::event::KeyCode` without pulling crossterm as a dep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

/// Modifier key mask for widget events.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_: bool,
}

// ---------------------------------------------------------------------------
// C ABI: raw-pointer helpers for widget loading.
//
// The host calls `cmdash_widget_create` from the loaded `.so`, which
// returns a `*mut c_void` (erased `Box<dyn CmdashWidget>`). The host
// then reinterprets this as `Box<dyn CmdashWidget>` using
// [`widget_from_raw`]. The widget `.so` produces the pointer via
// [`widget_into_raw`].
//
// SAFETY: both sides must agree on the fat-pointer layout for
// `Box<dyn CmdashWidget>`. This is true when both are compiled with
// the same Rust toolchain and the same `cmdash-widget-sdk` version
// (so the vtable shape is identical). This is the same contract as
// every Rust dynamic-loading plugin system (e.g. `tracing`,
// `libloading` examples, Bevy plugins).
// ---------------------------------------------------------------------------

/// Erase a `Box<dyn CmdashWidget>` into a thin raw pointer for FFI.
///
/// Uses the double-box pattern: `Box<Box<dyn CmdashWidget>>` is a
/// thin pointer (`*mut Box<dyn CmdashWidget>` is pointer-sized), so
/// it can be cast to/from `*mut c_void` without losing the fat
/// pointer (vtable + data) that lives inside the inner `Box`.
///
/// # Safety
///
/// The caller must eventually reconstitute the `Box` via
/// [`widget_from_raw`] to avoid a memory leak. The pointer must not
/// be used after reconstitution.
pub unsafe fn widget_into_raw(w: Box<dyn CmdashWidget>) -> *mut c_void {
    let boxed: Box<Box<dyn CmdashWidget>> = Box::new(w);
    Box::into_raw(boxed) as *mut c_void
}

/// Reconstitute a `Box<dyn CmdashWidget>` from a thin raw pointer
/// returned by [`widget_into_raw`].
///
/// # Safety
///
/// `ptr` must have been produced by [`widget_into_raw`] and must not
/// have been reconstituted already.
pub unsafe fn widget_from_raw(ptr: *mut c_void) -> Box<dyn CmdashWidget> {
    // SAFETY: the pointer was produced by Box::into_raw on a
    // Box<Box<dyn CmdashWidget>> via widget_into_raw; the inner
    // Box<dyn CmdashWidget> preserves the fat pointer (data +
    // vtable) across the thin-pointer FFI boundary.
    let boxed: Box<Box<dyn CmdashWidget>> = Box::from_raw(ptr as *mut Box<dyn CmdashWidget>);
    *boxed
}

/// Export macro for widget `.so` authors. Generates the
/// `cmdash_widget_create` C-ABI function that the host's
/// `libloading` loader calls at startup.
///
/// The type `$Widget` must implement `CmdashWidget + Default + Send +
/// 'static`. The generated function:
///
/// 1. Checks that the host-supplied ABI version matches
///    [`CMDASH_WIDGET_ABI_VERSION`].
/// 2. Creates a `Box<$Widget>` via `Default::default()`.
/// 3. Erases it to `*mut c_void` via [`widget_into_raw`].
///
/// # Example
///
/// ```ignore
/// use cmdash_widget_sdk::{cmdash_widget_export, CmdashWidget, WidgetEvent};
///
/// #[derive(Default)]
/// struct Clock;
///
/// impl CmdashWidget for Clock {
///     fn name(&self) -> &str { "clock" }
///     fn render(&mut self, area: ratatui::layout::Rect, frame: &mut ratatui::Frame) {
///         use ratatui::widgets::{Block, Borders, Paragraph};
///         frame.render_widget(Paragraph::new("12:00"), area);
///     }
/// }
///
/// cmdash_widget_export!(Clock);
/// ```
#[macro_export]
macro_rules! cmdash_widget_export {
    ($Widget:ty) => {
        /// C-ABI entry point called by the cmdash host's
        /// `libloading` loader. Returns a thin raw pointer to a
        /// heap-allocated `$Widget` (double-boxed so the thin
        /// pointer can cross the FFI boundary without losing the
        /// fat-pointer vtable). Returns `std::ptr::null_mut()` on
        /// ABI version mismatch.
        ///
        /// # Safety
        ///
        /// The returned pointer must be reconstituted exactly once
        /// via `cmdash_widget_sdk::widget_from_raw`.
        #[no_mangle]
        pub unsafe extern "C" fn cmdash_widget_create(abi_version: u32) -> *mut std::ffi::c_void {
            if abi_version != $crate::CMDASH_WIDGET_ABI_VERSION {
                return std::ptr::null_mut();
            }
            let widget: Box<dyn $crate::CmdashWidget> =
                Box::new(<$Widget as std::default::Default>::default());
            $crate::widget_into_raw(widget)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trivial widget that records its last render area.
    #[derive(Default)]
    struct TestWidget {
        last_area: Option<(u16, u16)>,
    }

    impl CmdashWidget for TestWidget {
        fn name(&self) -> &str {
            "test"
        }
        fn render(&mut self, area: ratatui::layout::Rect, _frame: &mut ratatui::Frame) {
            self.last_area = Some((area.width, area.height));
        }
    }

    /// ABI version constant is 1.
    #[test]
    fn abi_version_is_1() {
        assert_eq!(CMDASH_WIDGET_ABI_VERSION, 1);
    }

    /// `widget_into_raw` / `widget_from_raw` round-trip preserves
    /// the widget's identity.
    #[test]
    fn raw_pointer_round_trip_preserves_widget() {
        let widget: Box<dyn CmdashWidget> = Box::new(TestWidget::default());
        let name_before = widget.name().to_string();
        let raw = unsafe { widget_into_raw(widget) };
        assert!(!raw.is_null());
        let restored = unsafe { widget_from_raw(raw) };
        assert_eq!(restored.name(), name_before);
    }

    /// `WidgetEvent::Key` carries code and modifiers.
    #[test]
    fn widget_event_key_round_trip() {
        let evt = WidgetEvent::Key {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers {
                ctrl: true,
                ..KeyModifiers::default()
            },
        };
        match &evt {
            WidgetEvent::Key { code, modifiers } => {
                assert_eq!(*code, KeyCode::Char('a'));
                assert!(modifiers.ctrl);
                assert!(!modifiers.alt);
            }
            _ => panic!("expected Key event"),
        }
    }

    /// `WidgetEvent::Resize` carries width and height.
    #[test]
    fn widget_event_resize() {
        let evt = WidgetEvent::Resize {
            width: 80,
            height: 24,
        };
        assert_eq!(
            evt,
            WidgetEvent::Resize {
                width: 80,
                height: 24,
            }
        );
    }

    /// `KeyModifiers::default()` is all-false.
    #[test]
    fn key_modifiers_default_is_all_false() {
        let m = KeyModifiers::default();
        assert!(!m.ctrl);
        assert!(!m.shift);
        assert!(!m.alt);
        assert!(!m.super_);
    }
}
