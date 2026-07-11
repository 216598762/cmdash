//! Line-delimited script-widget frame protocol: cmdash <-> executable.
//!
//! ## Wire format
//!
//! All messages are line-delimited UTF-8. Key-value pairs use `key=value`
//! syntax separated by spaces. Keys are lowercase ASCII.
//!
//! ### Host → Script (stdin)
//!
//! | Message     | Format                                                    |
//! |-------------|-----------------------------------------------------------|
//! | Frame       | `FRAME width=<u16> height=<u16> gen=<u64>`               |
//! | Key event   | `KEY key=<name> [mod=<mods>]`                             |
//! | Resize      | `RESIZE width=<u16> height=<u16>`                         |
//! | Focus       | `FOCUS gained|lost`                                       |
//! | Mouse       | `MOUSE x=<u16> y=<u16> kind=<kind> btn=<btn>`            |
//!
//! ### Script → Host (stdout)
//!
//! Frame response: `FRAME width=<u16> height=<u16>` followed by ANSI
//! text lines until the next FRAME header or EOF.
//!
//! ## Version
//!
//! [`PROTOCOL_VERSION`] = 1 — line + ANSI only. Pixel-bitmap frame
//! mode is a future goal.
//!
//! ## Implementation
//!
//! The host spawns the script with piped stdin/stdout. A reader thread
//! reads frames from stdout; the render loop sends FRAME requests
//! and picks up the latest response via a non-blocking channel.

/// Protocol version. Checked at startup for compatibility.
pub const PROTOCOL_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Host → Script messages
// ---------------------------------------------------------------------------

/// Messages sent from the host to the script process via stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMsg {
    /// Request a frame render at the given cell-grid dimensions.
    /// `gen` is a monotonically increasing generation counter so the
    /// script can correlate requests with responses.
    Frame { width: u16, height: u16, gen: u64 },
    /// Forward a key event. `key` is the key name (e.g. `"a"`,
    /// `"enter"`, `"up"`). `modifiers` is a `+`-separated list
    /// (e.g. `"ctrl"`, `"ctrl+shift"`).
    Key { key: String, modifiers: String },
    /// Notify the script of a terminal resize.
    Resize { width: u16, height: u16 },
    /// Forward a mouse event.
    #[allow(dead_code)] // v2: mouse forwarding not yet wired in ScriptWidget.
    Mouse {
        x: u16,
        y: u16,
        kind: String,
        btn: String,
    },
    /// Notify the script of a focus state change.
    Focus { gained: bool },
}

impl HostMsg {
    /// Serialize to the wire format (line-delimited, without trailing newline).
    pub fn to_wire(&self) -> String {
        match self {
            HostMsg::Frame { width, height, gen } => {
                format!("FRAME width={width} height={height} gen={gen}")
            }
            HostMsg::Key { key, modifiers } => {
                if modifiers.is_empty() {
                    format!("KEY key={key}")
                } else {
                    format!("KEY key={key} mod={modifiers}")
                }
            }
            HostMsg::Resize { width, height } => {
                format!("RESIZE width={width} height={height}")
            }
            HostMsg::Mouse { x, y, kind, btn } => {
                format!("MOUSE x={x} y={y} kind={kind} btn={btn}")
            }
            HostMsg::Focus { gained } => {
                if *gained {
                    "FOCUS gained".into()
                } else {
                    "FOCUS lost".into()
                }
            }
        }
    }
}

impl std::fmt::Display for HostMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_wire())
    }
}

// ---------------------------------------------------------------------------
// Script → Host frame response
// ---------------------------------------------------------------------------

/// A parsed frame response from the script process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameResponse {
    pub width: u16,
    pub height: u16,
    /// ANSI text lines (excluding the FRAME header line).
    pub lines: Vec<String>,
}

impl FrameResponse {
    /// Parse a `FRAME width=W height=H` header line.
    /// Returns `None` if the line is not a valid FRAME header.
    pub fn parse_header(line: &str) -> Option<Self> {
        let line = line.trim();
        let rest = line.strip_prefix("FRAME ")?;
        let mut width = 0u16;
        let mut height = 0u16;
        for part in rest.split_whitespace() {
            if let Some(val) = part.strip_prefix("width=") {
                width = val.parse().ok()?;
            } else if let Some(val) = part.strip_prefix("height=") {
                height = val.parse().ok()?;
            }
        }
        Some(FrameResponse {
            width,
            height,
            lines: Vec::new(),
        })
    }

    /// Returns `true` if a line is a FRAME header (start of a new frame).
    pub fn is_frame_header(line: &str) -> bool {
        line.trim_start().strip_prefix("FRAME ").is_some()
    }
}

// ---------------------------------------------------------------------------
// Wire-format parsing (HostMsg from script stdin)
// ---------------------------------------------------------------------------

/// Parse a single host message line from the wire format.
/// Returns `None` for unrecognized or malformed lines.
pub fn parse_host_msg(line: &str) -> Option<HostMsg> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("FRAME ") {
        let mut width = 0u16;
        let mut height = 0u16;
        let mut gen = 0u64;
        for part in rest.split_whitespace() {
            if let Some(val) = part.strip_prefix("width=") {
                width = val.parse().ok()?;
            } else if let Some(val) = part.strip_prefix("height=") {
                height = val.parse().ok()?;
            } else if let Some(val) = part.strip_prefix("gen=") {
                gen = val.parse().ok()?;
            }
        }
        Some(HostMsg::Frame { width, height, gen })
    } else if let Some(rest) = line.strip_prefix("KEY ") {
        let mut key = String::new();
        let mut modifiers = String::new();
        for part in rest.split_whitespace() {
            if let Some(val) = part.strip_prefix("key=") {
                key = val.to_string();
            } else if let Some(val) = part.strip_prefix("mod=") {
                modifiers = val.to_string();
            }
        }
        if key.is_empty() {
            return None;
        }
        Some(HostMsg::Key { key, modifiers })
    } else if let Some(rest) = line.strip_prefix("RESIZE ") {
        let mut width = 0u16;
        let mut height = 0u16;
        for part in rest.split_whitespace() {
            if let Some(val) = part.strip_prefix("width=") {
                width = val.parse().ok()?;
            } else if let Some(val) = part.strip_prefix("height=") {
                height = val.parse().ok()?;
            }
        }
        Some(HostMsg::Resize { width, height })
    } else if let Some(rest) = line.strip_prefix("MOUSE ") {
        let mut x = 0u16;
        let mut y = 0u16;
        let mut kind = String::new();
        let mut btn = String::new();
        for part in rest.split_whitespace() {
            if let Some(val) = part.strip_prefix("x=") {
                x = val.parse().ok()?;
            } else if let Some(val) = part.strip_prefix("y=") {
                y = val.parse().ok()?;
            } else if let Some(val) = part.strip_prefix("kind=") {
                kind = val.to_string();
            } else if let Some(val) = part.strip_prefix("btn=") {
                btn = val.to_string();
            }
        }
        Some(HostMsg::Mouse { x, y, kind, btn })
    } else if let Some(rest) = line.strip_prefix("FOCUS ") {
        let gained = match rest.trim() {
            "gained" => true,
            "lost" => false,
            _ => return None,
        };
        Some(HostMsg::Focus { gained })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // HostMsg serialization
    // -----------------------------------------------------------------------

    #[test]
    fn host_msg_frame_to_wire() {
        let msg = HostMsg::Frame {
            width: 80,
            height: 24,
            gen: 42,
        };
        assert_eq!(msg.to_wire(), "FRAME width=80 height=24 gen=42");
    }

    #[test]
    fn host_msg_key_with_modifiers() {
        let msg = HostMsg::Key {
            key: "a".into(),
            modifiers: "ctrl".into(),
        };
        assert_eq!(msg.to_wire(), "KEY key=a mod=ctrl");
    }

    #[test]
    fn host_msg_key_without_modifiers() {
        let msg = HostMsg::Key {
            key: "enter".into(),
            modifiers: String::new(),
        };
        assert_eq!(msg.to_wire(), "KEY key=enter");
    }

    #[test]
    fn host_msg_resize_to_wire() {
        let msg = HostMsg::Resize {
            width: 120,
            height: 40,
        };
        assert_eq!(msg.to_wire(), "RESIZE width=120 height=40");
    }

    #[test]
    fn host_msg_mouse_to_wire() {
        let msg = HostMsg::Mouse {
            x: 10,
            y: 5,
            kind: "press".into(),
            btn: "left".into(),
        };
        assert_eq!(msg.to_wire(), "MOUSE x=10 y=5 kind=press btn=left");
    }

    // -----------------------------------------------------------------------
    // HostMsg parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_frame_msg() {
        let msg = parse_host_msg("FRAME width=80 height=24 gen=1").unwrap();
        assert_eq!(
            msg,
            HostMsg::Frame {
                width: 80,
                height: 24,
                gen: 1,
            }
        );
    }

    #[test]
    fn parse_key_msg_with_mod() {
        let msg = parse_host_msg("KEY key=a mod=ctrl").unwrap();
        assert_eq!(
            msg,
            HostMsg::Key {
                key: "a".into(),
                modifiers: "ctrl".into(),
            }
        );
    }

    #[test]
    fn parse_key_msg_without_mod() {
        let msg = parse_host_msg("KEY key=enter").unwrap();
        assert_eq!(
            msg,
            HostMsg::Key {
                key: "enter".into(),
                modifiers: String::new(),
            }
        );
    }

    #[test]
    fn parse_key_msg_missing_key_returns_none() {
        assert!(parse_host_msg("KEY mod=ctrl").is_none());
    }

    #[test]
    fn parse_resize_msg() {
        let msg = parse_host_msg("RESIZE width=120 height=40").unwrap();
        assert_eq!(
            msg,
            HostMsg::Resize {
                width: 120,
                height: 40,
            }
        );
    }

    #[test]
    fn parse_mouse_msg() {
        let msg = parse_host_msg("MOUSE x=10 y=5 kind=press btn=left").unwrap();
        assert_eq!(
            msg,
            HostMsg::Mouse {
                x: 10,
                y: 5,
                kind: "press".into(),
                btn: "left".into(),
            }
        );
    }

    #[test]
    fn parse_unknown_msg_returns_none() {
        assert!(parse_host_msg("UNKNOWN foo=bar").is_none());
    }

    #[test]
    fn host_msg_focus_gained_to_wire() {
        let msg = HostMsg::Focus { gained: true };
        assert_eq!(msg.to_wire(), "FOCUS gained");
    }

    #[test]
    fn host_msg_focus_lost_to_wire() {
        let msg = HostMsg::Focus { gained: false };
        assert_eq!(msg.to_wire(), "FOCUS lost");
    }

    #[test]
    fn parse_focus_gained_msg() {
        let msg = parse_host_msg("FOCUS gained").unwrap();
        assert_eq!(msg, HostMsg::Focus { gained: true });
    }

    #[test]
    fn parse_focus_lost_msg() {
        let msg = parse_host_msg("FOCUS lost").unwrap();
        assert_eq!(msg, HostMsg::Focus { gained: false });
    }

    #[test]
    fn parse_focus_unknown_state_returns_none() {
        assert!(parse_host_msg("FOCUS maybe").is_none());
    }

    #[test]
    fn parse_empty_msg_returns_none() {
        assert!(parse_host_msg("").is_none());
    }

    // -----------------------------------------------------------------------
    // FrameResponse parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_frame_header_basic() {
        let resp = FrameResponse::parse_header("FRAME width=80 height=24").unwrap();
        assert_eq!(resp.width, 80);
        assert_eq!(resp.height, 24);
        assert!(resp.lines.is_empty());
    }

    #[test]
    fn parse_frame_header_ignores_extra_fields() {
        let resp = FrameResponse::parse_header("FRAME width=80 height=24 gen=99").unwrap();
        assert_eq!(resp.width, 80);
        assert_eq!(resp.height, 24);
    }

    #[test]
    fn parse_frame_header_not_frame_returns_none() {
        assert!(FrameResponse::parse_header("NOTFRAME width=80").is_none());
    }

    #[test]
    fn is_frame_header_detects_headers() {
        assert!(FrameResponse::is_frame_header("FRAME width=80 height=24"));
        assert!(FrameResponse::is_frame_header("  FRAME width=80 height=24"));
        assert!(!FrameResponse::is_frame_header("hello world"));
        assert!(!FrameResponse::is_frame_header(""));
    }

    // -----------------------------------------------------------------------
    // Round-trip: serialize → parse
    // -----------------------------------------------------------------------

    #[test]
    fn round_trip_host_msg() {
        let msgs = vec![
            HostMsg::Frame {
                width: 100,
                height: 50,
                gen: 7,
            },
            HostMsg::Key {
                key: "q".into(),
                modifiers: "alt".into(),
            },
            HostMsg::Resize {
                width: 200,
                height: 100,
            },
            HostMsg::Mouse {
                x: 3,
                y: 7,
                kind: "click".into(),
                btn: "right".into(),
            },
        ];
        for msg in &msgs {
            let wire = msg.to_wire();
            let parsed = parse_host_msg(&wire).expect("parse should succeed");
            assert_eq!(&parsed, msg);
        }
    }

    #[test]
    fn round_trip_focus_msgs() {
        let msgs = vec![
            HostMsg::Focus { gained: true },
            HostMsg::Focus { gained: false },
        ];
        for msg in &msgs {
            let wire = msg.to_wire();
            let parsed = parse_host_msg(&wire).expect("parse should succeed");
            assert_eq!(&parsed, msg);
        }
    }
}
