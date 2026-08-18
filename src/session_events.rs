//! Coordinator-owned session-event bus (Phase 17).
//!
//! Terminal sessions publish bounded lifecycle events (focus, title, line
//! output, exit) and script widgets subscribe to them via a bounded queue.
//! Subscribers deliver the events to their spawned script on fd 3 in either a
//! newline-delimited text format or JSON. The bus also carries the read-only
//! session-context snapshot that script widgets surface as `CMDASH_SESSION_*`
//! environment variables at spawn.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};

use crate::state::SessionId;

/// Default depth of a subscriber's event queue; older events are dropped when
/// a subscriber falls behind.
pub const DEFAULT_SESSION_EVENT_CAPACITY: usize = 1024;

/// An event delivered to subscribing script widgets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionEvent {
    pub id: SessionId,
    pub kind: SessionEventKind,
}

impl SessionEvent {
    pub fn new(id: SessionId, kind: SessionEventKind) -> Self {
        Self { id, kind }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEventKind {
    /// A terminal session became the focused session.
    Focus { title: String },
    /// A terminal session changed its (OSC 0/2) window title.
    Title { title: String },
    /// A newline-delimited line of terminal output.
    Line { text: String },
    /// A terminal session's child exited.
    Exit { code: Option<i32> },
}

/// How a script widget wants its fd-3 session events formatted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SessionEventMode {
    /// No fd-3 pipe is opened and no events are delivered.
    #[default]
    Off,
    /// Newline-delimited `session <id> <kind> ...` lines.
    Text,
    /// Newline-delimited JSON objects.
    Json,
}

impl SessionEventMode {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("off") => Ok(Self::Off),
            Some("text") => Ok(Self::Text),
            Some("json") => Ok(Self::Json),
            Some(other) => Err(format!(
                "widget session_events must be \"off\", \"text\", or \"json\", got {other:?}"
            )),
        }
    }

    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Formats a single event as one line (without a trailing newline).
pub fn format_session_event(event: &SessionEvent, mode: SessionEventMode) -> String {
    match mode {
        SessionEventMode::Text => match &event.kind {
            SessionEventKind::Focus { title } => {
                format!("session {} focus {title}", event.id.get())
            }
            SessionEventKind::Title { title } => {
                format!("session {} title {title}", event.id.get())
            }
            SessionEventKind::Line { text } => {
                format!("session {} line {text}", event.id.get())
            }
            SessionEventKind::Exit { code } => match code {
                Some(code) => format!("session {} exit {code}", event.id.get()),
                None => format!("session {} exit", event.id.get()),
            },
        },
        SessionEventMode::Json => match &event.kind {
            SessionEventKind::Focus { title } => serde_json::json!({
                "session": event.id.get(),
                "event": "focus",
                "title": title,
            })
            .to_string(),
            SessionEventKind::Title { title } => serde_json::json!({
                "session": event.id.get(),
                "event": "title",
                "title": title,
            })
            .to_string(),
            SessionEventKind::Line { text } => serde_json::json!({
                "session": event.id.get(),
                "event": "line",
                "text": text,
            })
            .to_string(),
            SessionEventKind::Exit { code } => serde_json::json!({
                "session": event.id.get(),
                "event": "exit",
                "code": code,
            })
            .to_string(),
        },
        SessionEventMode::Off => String::new(),
    }
}

/// A read-only snapshot of the session context a widget surfaces at spawn.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionContextSnapshot {
    pub count: usize,
    pub focused_id: Option<SessionId>,
    pub focused_title: Option<String>,
}

#[derive(Default)]
struct SessionContext {
    count: usize,
    focused: Option<(SessionId, String)>,
    titles: BTreeMap<SessionId, String>,
}

/// Per-subscriber state: a bounded queue plus an overflow flag.
struct SubscriberState {
    queue: Mutex<VecDeque<SessionEvent>>,
    capacity: usize,
    overflow: AtomicBool,
}

impl SubscriberState {
    fn new(capacity: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
            overflow: AtomicBool::new(false),
        }
    }
}

struct BusInner {
    subscribers: Mutex<Vec<Weak<SubscriberState>>>,
    context: Mutex<SessionContext>,
}

/// The coordinator-owned event bus shared between terminal sessions (publish)
/// and script widgets (subscribe).
#[derive(Clone)]
pub struct SessionEventBus {
    inner: Arc<BusInner>,
}

impl Default for SessionEventBus {
    fn default() -> Self {
        Self {
            inner: Arc::new(BusInner {
                subscribers: Mutex::new(Vec::new()),
                context: Mutex::new(SessionContext::default()),
            }),
        }
    }
}

impl SessionEventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a subscriber with a bounded queue and returns its drain handle.
    /// The subscription lives as long as the returned receiver.
    pub fn subscribe(&self, capacity: usize) -> SessionEventReceiver {
        let state = Arc::new(SubscriberState::new(capacity));
        self.inner
            .subscribers
            .lock()
            .expect("session-event subscriber list poisoned")
            .push(Arc::downgrade(&state));
        SessionEventReceiver { state }
    }

    /// Whether any live subscriber is currently registered.
    pub fn has_subscribers(&self) -> bool {
        self.prune();
        !self
            .inner
            .subscribers
            .lock()
            .expect("session-event subscriber list poisoned")
            .is_empty()
    }

    /// Publishes an event to every subscriber (bounded, dropping the oldest
    /// event and recording overflow when a subscriber falls behind). Title
    /// events additionally update the shared session-context title map.
    pub fn publish(&self, event: SessionEvent) {
        if let SessionEventKind::Title { title } = &event.kind {
            let mut context = self.inner.context.lock().expect("session context poisoned");
            context.titles.insert(event.id, title.clone());
            if let Some((focused_id, _)) = &context.focused
                && *focused_id == event.id
            {
                context.focused = Some((*focused_id, title.clone()));
            }
        }
        self.prune();
        let subscribers = self
            .inner
            .subscribers
            .lock()
            .expect("session-event subscriber list poisoned");
        for weak in subscribers.iter() {
            if let Some(state) = weak.upgrade() {
                let mut queue = state.queue.lock().expect("session-event queue poisoned");
                if queue.len() >= state.capacity {
                    queue.pop_front();
                    state.overflow.store(true, Ordering::Release);
                }
                queue.push_back(event.clone());
            }
        }
    }

    /// Updates the read-only session context (session count and focused
    /// session) exposed to widgets at spawn.
    pub fn update_context(&self, count: usize, focused: Option<(SessionId, String)>) {
        let mut context = self.inner.context.lock().expect("session context poisoned");
        context.count = count;
        context.focused = focused;
    }

    /// The title most recently reported for a session, if any.
    pub fn title_of(&self, id: SessionId) -> Option<String> {
        self.inner
            .context
            .lock()
            .expect("session context poisoned")
            .titles
            .get(&id)
            .cloned()
    }

    /// The current read-only session context snapshot.
    pub fn context_snapshot(&self) -> SessionContextSnapshot {
        let context = self.inner.context.lock().expect("session context poisoned");
        SessionContextSnapshot {
            count: context.count,
            focused_id: context.focused.as_ref().map(|(id, _)| *id),
            focused_title: context.focused.as_ref().map(|(_, title)| title.clone()),
        }
    }

    /// Drops subscribers whose receivers have been released.
    fn prune(&self) {
        self.inner
            .subscribers
            .lock()
            .expect("session-event subscriber list poisoned")
            .retain(|weak| weak.strong_count() > 0);
    }
}

/// A subscriber's drain handle. Polled by a script widget each update to pull
/// queued events and deliver them to its spawned process on fd 3.
pub struct SessionEventReceiver {
    state: Arc<SubscriberState>,
}

impl SessionEventReceiver {
    /// Takes all currently queued events, oldest first.
    pub fn drain(&self) -> Vec<SessionEvent> {
        let mut queue = self.state.queue.lock().expect("session-event queue poisoned");
        std::mem::take(&mut *queue).into_iter().collect()
    }

    /// Returns and clears the overflow flag (set when events were dropped
    /// because the subscriber fell behind).
    pub fn take_overflow(&self) -> bool {
        self.state.overflow.swap(false, Ordering::AcqRel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_to_all_subscribers_in_order() {
        let bus = SessionEventBus::new();
        let a = bus.subscribe(4);
        let b = bus.subscribe(4);
        bus.publish(SessionEvent::new(
            SessionId::new(1),
            SessionEventKind::Focus {
                title: "shell".to_owned(),
            },
        ));
        bus.publish(SessionEvent::new(
            SessionId::new(1),
            SessionEventKind::Line {
                text: "hello".to_owned(),
            },
        ));
        assert_eq!(a.drain().len(), 2);
        assert_eq!(b.drain().len(), 2);
    }

    #[test]
    fn drops_oldest_events_when_a_subscriber_falls_behind() {
        let bus = SessionEventBus::new();
        let receiver = bus.subscribe(2);
        for index in 0..4 {
            bus.publish(SessionEvent::new(
                SessionId::new(1),
                SessionEventKind::Line {
                    text: index.to_string(),
                },
            ));
        }
        let drained = receiver.drain();
        assert_eq!(
            drained
                .iter()
                .map(|event| match &event.kind {
                    SessionEventKind::Line { text } => text.as_str(),
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>(),
            vec!["2", "3"]
        );
        assert!(receiver.take_overflow());
        assert!(!receiver.take_overflow());
    }

    #[test]
    fn title_events_update_the_shared_context_title_map() {
        let bus = SessionEventBus::new();
        bus.publish(SessionEvent::new(
            SessionId::new(7),
            SessionEventKind::Title {
                title: "nvim".to_owned(),
            },
        ));
        assert_eq!(bus.title_of(SessionId::new(7)).as_deref(), Some("nvim"));
        assert_eq!(bus.title_of(SessionId::new(8)), None);
    }

    #[test]
    fn context_snapshot_reflects_updates() {
        let bus = SessionEventBus::new();
        bus.update_context(3, Some((SessionId::new(2), "shell".to_owned())));
        assert_eq!(
            bus.context_snapshot(),
            SessionContextSnapshot {
                count: 3,
                focused_id: Some(SessionId::new(2)),
                focused_title: Some("shell".to_owned()),
            }
        );
    }

    #[test]
    fn text_and_json_formats_are_stable() {
        let focus = SessionEvent::new(
            SessionId::new(1),
            SessionEventKind::Focus {
                title: "bash".to_owned(),
            },
        );
        assert_eq!(
            format_session_event(&focus, SessionEventMode::Text),
            "session 1 focus bash"
        );
        assert_eq!(
            format_session_event(&focus, SessionEventMode::Json),
            r#"{"event":"focus","session":1,"title":"bash"}"#
        );

        let exit = SessionEvent::new(SessionId::new(1), SessionEventKind::Exit { code: Some(0) });
        assert_eq!(
            format_session_event(&exit, SessionEventMode::Text),
            "session 1 exit 0"
        );
        let exit = SessionEvent::new(SessionId::new(1), SessionEventKind::Exit { code: None });
        assert_eq!(
            format_session_event(&exit, SessionEventMode::Text),
            "session 1 exit"
        );
    }

    #[test]
    fn mode_parser_accepts_known_values() {
        assert_eq!(SessionEventMode::parse(None), Ok(SessionEventMode::Off));
        assert_eq!(
            SessionEventMode::parse(Some("off")),
            Ok(SessionEventMode::Off)
        );
        assert_eq!(
            SessionEventMode::parse(Some("text")),
            Ok(SessionEventMode::Text)
        );
        assert_eq!(
            SessionEventMode::parse(Some("json")),
            Ok(SessionEventMode::Json)
        );
        assert!(SessionEventMode::parse(Some("yaml")).is_err());
    }
}
