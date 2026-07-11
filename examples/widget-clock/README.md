# widget-clock

An example cmdash cdylib widget that displays a real-time clock. This
widget validates the **full widget loading pipeline** end-to-end:

1. Host loads `libwidget_clock.so` via `libloading`
2. Host calls `cmdash_widget_create(ABI_VERSION)` → `Box<dyn CmdashWidget>`
3. Host calls `widget.render(area, frame)` once per frame (~30 fps)
4. Host forwards `WidgetEvent` on focus/key events

## Build

```sh
cargo build -p widget-clock --release
```

This produces `target/release/libwidget_clock.so`.

## Install

```sh
mkdir -p ~/.config/cmdash/widgets/widget-clock
cp target/release/libwidget_clock.so ~/.config/cmdash/widgets/widget-clock/
```

## Run

```sh
cmdash --config=examples/06-widget-clock.kdl
```

Or copy the layout into your own config:

```kdl
layout {
    split axis=horizontal ratio=0.3 {
        pane kind=widget ref-name="widget-clock" label="clock"
        pane kind=shell label="shell"
    }
}
```

## Development

The widget implements the `CmdashWidget` trait from `cmdash-widget-sdk`:

- **`render()`** — draws the current HH:MM:SS into a bordered ratatui panel
- **`on_event()`** — tracks focus state for a visual border highlight
- **`cmdash_widget_export!(ClockWidget)`** — generates the C-ABI entry point

See `cmdash-widget-sdk/src/lib.rs` for the full ABI contract and
`cmdash/src/main.rs` for the host-side loading logic.
