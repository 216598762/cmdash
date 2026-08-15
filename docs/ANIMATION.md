# Animation and motion

Phase 12 adds optional, retained-scene motion without making animation necessary
for correctness, input, terminal sessions, or graphics. The complete motion
contract lives in this document; appearance, configuration, and widget guides
link here rather than duplicating animation options.

## Design guarantees

- Animation is disabled by default and is safe to disable at any time.
- The UI coordinator owns scheduling and rendering. Widgets and plugins never
  create animation threads or write terminal escape sequences.
- Animation state is separate from PTY, emulator, scrollback, cursor, selection,
  and graphics state.
- Every animation produces an ordinary clipped `Scene`; the compositor and
  backend retain ownership of frame diffs and terminal output.
- Limits are enforced before work is started: the default maximum is 16
  concurrent animations and the configuration maximum is 128.
- Hidden tabs, closed panes, failed widgets, reloads, and shutdown do not retain
  animation work for removed owners.

## Configuration

Animation is opt-in at the workspace level:

```toml
[animation]
enabled = true
reduced_motion = false
duration_ms = 180
delay_ms = 0
easing = "easeinout" # linear, easein, easeout, easeinout, step
repeat = 0
direction = "normal" # normal or alternate
fill = "forwards"    # none or forwards
max_concurrent = 16
```

The defaults are conservative: `enabled = false`, `duration_ms = 180`, no
repeat, and a maximum of 16 concurrent effects. Valid duration and delay values
are 1–60000 and 0–60000 milliseconds respectively. `repeat` is bounded by the
wire/runtime model, and `max_concurrent` is bounded to 1–128. Invalid values
reject a candidate configuration without replacing the active workspace.

`reduced_motion = true` disables active transitions and resolves them directly
to their final static state. This is the preferred accessibility setting. A
runtime can also pause and resume the coordinator-owned animation manager; a
paused animation retains its elapsed progress and does not wake the UI.

### Per-widget options

Widget settings may override the global timing for a specific instance:

```toml
[[workspace.widgets]]
id = 10
type = "clock"

[workspace.widgets.settings]
animation = "true"
animation_duration_ms = "240"
animation_delay_ms = "20"
animation_easing = "ease-out"
animation_repeat = "2"
animation_direction = "alternate"
animation_fill = "forwards"
```

`animation = "false"` disables effects for that widget. Per-widget durations
and delays are bounded to 1–60000 milliseconds, and repeats are bounded to 32.
Runtime-created panes inherit the source widget's settings. Unknown animation
values are validation errors rather than silently falling back to a different
motion profile.

## Retained model

The public animation primitives are intentionally small and deterministic:

- `AnimationSpec` describes delay, duration, easing, repeat count, direction,
  and fill mode;
- `AnimationKey` identifies coordinator-owned effects such as focus, tabs, and
  panes, with room for widget/surface/overlay keys;
- `AnimationSample` contains integer progress in the range `0..=1000` and an
  active flag;
- `AnimationManager` stores active timelines, enforces the concurrent budget,
  supports cancellation and pause/resume, and returns the next wakeup delay;
- `AnimationFrame` is passed to widget scene construction without exposing
  timers or terminal handles.

Starting a key interrupts/replaces its previous timeline. A completed timeline
is removed unless its fill mode retains the final sample. A budget-full start is
rejected without affecting existing effects. Reduced motion resolves a new
start to its completed sample immediately.

The initial effects are deliberately bounded: focus changes, tab switches, pane
creation/closure, and the associated retained surface transition. The scene
path applies a terminal-safe dim/static presentation while progress changes;
there is no alpha blending, unsupported escape sequence, or cross-surface
compositing. Future effects can add richer style interpolation while retaining
these ownership and quota rules.

## Scheduler and render path

The existing maintenance waker is shared by cursor blinking and animations.
It sleeps until the nearest maintenance, cursor, or animation deadline and
sends a coordinator event. There is no fixed-rate PTY polling and no per-widget
worker:

```text
input / PTY / filesystem / wakeup
              │
              ▼
       UI coordinator
              │
       AppState::dispatch
              │
       AnimationManager::advance(clock)
              │
       widget scene + AnimationFrame
              │
       Compositor -> backend diff
```

An active animation requests another frame only while it has progress to
advance. Simultaneous effects share the same coordinator wakeup. PTY readers
remain independently wakeable, and a cursor animation never polls hidden or
unfocused sessions.

## Terminal cursor behavior

The terminal emulator remains authoritative for cursor shape and terminal mode.
cmdash controls only the presentation blink for the focused, visible terminal
pane:

```toml
[[workspace.widgets]]
id = 11
type = "terminal"
command = "sh"

[workspace.widgets.settings]
cursor_blink = "true"
cursor_blink_interval_ms = "500"
```

`cursor_blink` defaults to `true`; intervals must be between 50 and 60000
milliseconds. Set it to `false` for a static cursor. Keyboard input, PTY
output, cursor movement, focus changes, tab changes, and pane lifecycle events
reset the cursor to visible. Hidden tabs, inactive sessions, unfocused panes,
closed panes, and shutdown do not blink. Reduced motion should be paired with a
static cursor setting when a completely motion-free terminal presentation is
required.

The cursor phase is not part of the terminal emulator state and is not copied
between sessions. If the emulator hides its cursor, cmdash does not force it
visible.

## Borders, labels, overlays, and geometry

Motion changes presentation, not ownership or layout contracts. Borders and
labels remain clipped to the widget surface; `label = "never"` and
`border = "none"` continue to preserve the configured content geometry. Terminal
PTY sizing, mouse coordinates, selection, and graphics placements use the same
content rectangle during every animation frame. Overlays and pane transitions
are invalidated through the normal compositor path, so stale cells and image
layers cannot leak into neighboring surfaces.

Plugins receive the explicit `ANIMATION` capability bit only when the host
advertises it. The host owns scheduling, progress, limits, and shutdown. A
plugin cannot spawn animation workers, access a PTY, emit terminal escapes, or
retain a session/graphics resource after its widget is gone.

## Lifecycle and failure behavior

- **Hidden tab:** stop requesting frames for effects owned by the hidden branch;
  retained session state remains untouched.
- **Closed pane or removed widget:** cancel its effects before the runtime entry
  is shut down.
- **Failed widget:** keep the rest of the workspace responsive and do not let a
  failed effect block rendering.
- **Configuration reload:** validate the new animation section and widget
  settings before installing them; rejection leaves the active manager intact.
- **Shutdown:** stop scheduling before widget/session shutdown and release all
  manager entries with the application state.
- **Unsupported or slow terminal:** use ordinary cell styles and the static
  completed frame; malformed escape output is never a fallback.

## Testing contract

The implementation has deterministic tests for easing, delays, repeat and
alternate direction, completion/fill behavior, pause/resume, reduced motion,
concurrent budgets, per-widget parsing, and scene motion. The application
checks should also cover:

- focus, tab, and pane triggers and interruption/restart;
- hidden-tab and closed-pane cleanup;
- frame wakeups without fixed-rate PTY polling;
- cursor reset and static-cursor fallback;
- clipping and session-qualified graphics during transitions;
- reload rejection and plugin capability/limit behavior.

When adding a new effect, inject a deterministic `SystemTime` in the manager
test, keep the effect bounded, and verify both the animated and static paths.

## Related documents

- [Configuration](CONFIGURATION.md) — schema and safe reload behavior.
- [Appearance](APPEARANCE.md) — static themes, borders, and label geometry.
- [Widgets](WIDGETS.md) — widget lifecycle and scene ownership.
- [Architecture](ARCHITECTURE.md) — coordinator and compositor boundaries.
- [Roadmap](ROADMAP.md) — Phase 12 completion and future work.
