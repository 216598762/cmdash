#![cfg(unix)]

use std::{
    io::{Read, Write},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

const INITIAL_COLUMNS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const RESIZED_COLUMNS: u16 = 60;
const RESIZED_ROWS: u16 = 20;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

struct PtyOutput {
    receiver: Receiver<Vec<u8>>,
    bytes: Vec<u8>,
}

impl PtyOutput {
    fn new(mut reader: Box<dyn Read + Send>) -> Self {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(length) => {
                        if sender.send(buffer[..length].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            receiver,
            bytes: Vec::new(),
        }
    }

    fn wait_for(&mut self, needle: &[u8]) -> bool {
        let deadline = Instant::now() + READ_TIMEOUT;
        while Instant::now() < deadline {
            if self
                .bytes
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.receiver.recv_timeout(remaining) {
                Ok(chunk) => self.bytes.extend_from_slice(&chunk),
                Err(_) => break,
            }
        }
        self.bytes
            .windows(needle.len())
            .any(|window| window == needle)
    }
}

fn unique_fixture_path(extension: &str) -> std::path::PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "cmdash-binary-boot-{}-{timestamp}.{extension}",
        std::process::id()
    ))
}

fn write_fixture_script() -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = unique_fixture_path("sh");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf '\\033[1;1HBOOT-MARK'\nsleep 1\nprintf 'RESIZE-MARK:'\nstty size\nsleep 5\n",
    )
    .expect("could not write the terminal child fixture");
    let mut permissions = std::fs::metadata(&path)
        .expect("could not stat the terminal child fixture")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&path, permissions)
        .expect("could not make the terminal child fixture executable");
    path
}

fn write_fixture_config(script: &std::path::Path) -> std::path::PathBuf {
    let path = unique_fixture_path("toml");
    let source = format!(
        r#"version = 1

[[workspace.widgets]]
id = 1
type = "terminal"
title = " shell "
command = "{}"

[[workspace.widgets]]
id = 2
type = "widget"
title = " idle "
command = "while :; do sleep 5; done"

[workspace.layout]
type = "columns"
children = [
  {{ type = "leaf", widget = 1 }},
  {{ type = "leaf", widget = 2 }}
]
"#,
        script.display()
    );
    std::fs::write(&path, source).expect("could not write the binary boot config");
    path
}

type BootProcess = (
    Box<dyn MasterPty>,
    Box<dyn Write + Send>,
    PtyOutput,
    Box<dyn portable_pty::Child>,
);

fn spawn_cmdash(config: &std::path::Path) -> BootProcess {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: INITIAL_ROWS,
            cols: INITIAL_COLUMNS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("could not allocate the cmdash outer PTY");
    let reader = pair
        .master
        .try_clone_reader()
        .expect("could not clone the cmdash outer PTY reader");
    let writer = pair
        .master
        .take_writer()
        .expect("could not open the cmdash outer PTY writer");
    let binary = std::env::var_os("CARGO_BIN_EXE_cmdash")
        .expect("Cargo did not provide CARGO_BIN_EXE_cmdash");
    let mut command = CommandBuilder::new(binary);
    command.arg("--config");
    command.arg(config);
    command.env("CMDASH_KITTY_GRAPHICS", "0");
    command.env("TERM", "xterm-256color");
    let child = pair
        .slave
        .spawn_command(command)
        .expect("could not spawn the cmdash binary under a PTY");
    let output = PtyOutput::new(reader);
    (pair.master, writer, output, child)
}

#[test]
fn binary_boot_composes_terminal_output_resizes_the_child_and_exits() {
    let script = write_fixture_script();
    let config = write_fixture_config(&script);
    let result = catch_unwind(AssertUnwindSafe(|| {
        let (master, mut writer, mut output, mut child) = spawn_cmdash(&config);

        // The child writes at its local origin. The terminal widget has a
        // one-cell border, so the retained scene must be emitted at the exact
        // absolute outer coordinate (2,2), encoded by crossterm as CSI 2;2H.
        assert!(
            output.wait_for(b"BOOT-MARK"),
            "cmdash never composed the terminal marker; output={:?}",
            String::from_utf8_lossy(&output.bytes)
        );
        let marker = output
            .bytes
            .windows(b"BOOT-MARK".len())
            .position(|window| window == b"BOOT-MARK")
            .expect("marker was observed but could not be located");
        let cursor = output.bytes[..marker]
            .windows(b"\x1b[2;2H".len())
            .rposition(|window| window == b"\x1b[2;2H");
        assert!(
            cursor.is_some(),
            "terminal marker was not composed at absolute cell (2,2); output={:?}",
            String::from_utf8_lossy(&output.bytes)
        );

        master
            .resize(PtySize {
                rows: RESIZED_ROWS,
                cols: RESIZED_COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("could not resize the cmdash outer PTY");
        // Input wakes the coordinator after the outer resize. The child is
        // sleeping here; its subsequent `stty size` observes the new PTY
        // dimensions independently of application-key routing.
        writer
            .write_all(b"x")
            .expect("could not wake cmdash after the outer resize");
        assert!(
            output.wait_for(b"28"),
            "cmdash did not render the child's resize report; output={:?}",
            String::from_utf8_lossy(&output.bytes)
        );
        let resize_marker = output
            .bytes
            .windows(b"RESIZE-MARK:".len())
            .position(|window| window == b"RESIZE-MARK:")
            .expect("resize marker was not rendered");
        let after_resize = &output.bytes[resize_marker..];
        let rows = after_resize
            .windows(b"18".len())
            .position(|window| window == b"18")
            .expect("resized child did not report 18 rows");
        assert!(
            after_resize[rows..]
                .windows(b"28".len())
                .any(|window| window == b"28"),
            "resized child did not report the exact 28-column content width; output={:?}",
            String::from_utf8_lossy(after_resize)
        );

        // Click the second surface through the real SGR mouse-input path so
        // focus leaves the terminal. q is then handled by the application
        // command router and shuts the real binary down normally.
        writer
            .write_all(b"\x1b[<0;46;6M")
            .expect("could not send the cmdash focus click");
        thread::sleep(Duration::from_millis(100));
        writer
            .write_all(b"q")
            .expect("could not send the cmdash quit command");
        let deadline = Instant::now() + READ_TIMEOUT;
        while Instant::now() < deadline {
            match child.try_wait().expect("could not poll cmdash child") {
                Some(status) => {
                    assert!(status.success(), "cmdash exited unsuccessfully: {status:?}");
                    return;
                }
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        let tail_start = output.bytes.len().saturating_sub(1000);
        panic!(
            "cmdash did not exit after the application quit command; output tail={:?}",
            String::from_utf8_lossy(&output.bytes[tail_start..])
        );
    }));
    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_file(&script);
    if let Err(payload) = result {
        resume_unwind(payload);
    }
}
