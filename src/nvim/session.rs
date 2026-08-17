//! Impure half of the embedded-Neovim spike: spawns `nvim --embed`, speaks
//! msgpack-rpc over its stdin/stdout by hand (see the module doc for why
//! hand-rolled over `nvim-rs`), and keeps a shared [`GridState`] up to date
//! from a background reader thread.
//!
//! Threading model: one writer thread owns the child's stdin and a
//! monotonic request-id counter, draining an [`NvimCmd`] channel and
//! encoding each as an `nvim_*` msgpack-rpc request; one reader thread owns
//! stdout, decoding messages in a loop and applying `redraw` notification
//! batches to `grid` via [`crate::nvim::grid`], then calling
//! `egui::Context::request_repaint` once a batch's `flush` event lands.
//! Neither thread touches egui/GUI state beyond that repaint call -- all
//! rendering reads `grid` from the UI thread each frame.
//!
//! Dependency choice: hand-rolled `rmpv` + `std::thread` rather than
//! `nvim-rs` + `tokio`. `nvim-rs`'s handler trait is built around async
//! `Neovim<W>` request/response futures and its own event-loop future you
//! `tokio::spawn` -- for a spike that only ever needs to fire three
//! one-way commands (`Input`/`Resize`/`OpenFile`) and consume one
//! notification stream, that's a tokio runtime, a trait impl, and a
//! generic writer type pulled in to do what two plain threads and a
//! channel do just as well. rmpv already gives self-framing msgpack
//! encode/decode; nvim's RPC wire format is `[type, ...]` arrays with no
//! extra framing, so there's no protocol work `nvim-rs` was saving.

use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use rmpv::Value;

use crate::nvim::grid::{parse_redraw_batch, GridState, RedrawEvent};

/// A command the UI thread sends down to the nvim writer thread.
pub enum NvimCmd {
    /// Raw nvim key notation (already translated -- see
    /// `crate::ui::nvim_pane::translate_event_for_nvim`), sent via
    /// `nvim_input`.
    Input(String),
    /// The pane was resized to `cols`x`rows`; sent via
    /// `nvim_ui_try_resize`.
    Resize(u16, u16),
    /// Open `path` (relative to the session's cwd is fine; absolute works
    /// too) via `nvim_cmd`'s `args` array -- no shell/Vim-command escaping
    /// needed, unlike building a `:edit <path>` string by hand. `line`
    /// places the cursor there afterwards (1-based, matching `:line`
    /// semantics) via a follow-up `nvim_win_set_cursor`.
    OpenFile(PathBuf, Option<u64>),
    /// Run an arbitrary Ex command via `nvim_command` -- used for
    /// `--nvim-cmd` init commands (see `main.rs`) after every attach/
    /// respawn. Fire-and-forget like every other [`NvimCmd`]; errors
    /// aren't observed here (see [`NvimSession::call`] for the
    /// request/response path `--nvim-cmd` actually uses to log them).
    Ex(String),
}

/// Whether an `nvim` binary is on `PATH` -- gates `--nvim` falling back to
/// the built-in file viewer instead of failing to spawn.
pub fn nvim_available() -> bool {
    which::which("nvim").is_ok()
}

/// A live embedded-nvim session: owns the child process, the writer/reader
/// threads, and the shared grid they publish redraws into.
///
/// Liveness: `alive` starts `true` and flips to `false` (never back) the
/// moment either thread notices the process is gone -- the reader hits EOF
/// on `stdout`, or the writer fails to write/flush `stdin` (e.g. a broken
/// pipe after `nvim` exits, which on this platform surfaces as an `EPIPE`
/// `io::Error` rather than a signal, since Rust's runtime ignores
/// `SIGPIPE` for the process). Neither thread ever panics on child exit --
/// both simply stop their loop and flip the flag. `NvimSession::send` also
/// flips it if the command channel itself has closed (writer thread
/// already gone), covering the "wrote a bad frame and broke the pipe
/// ourselves" case too. This is the fix for the `ZZ` lockup: previously
/// nothing observed a dead child at all, so the UI kept believing the nvim
/// pane was live -- key events kept forwarding into a dead channel
/// (harmlessly swallowed, but with zero feedback) and the grid never
/// stopped showing its last frame before quit. See
/// [`crate::ui::eframe_app::VdiffApp`] for how `is_alive` now drives an
/// automatic return to the graph pane and a respawn-on-next-open.
pub struct NvimSession {
    child: Child,
    cmd_tx: Sender<NvimCmd>,
    grid: Arc<Mutex<GridState>>,
    alive: Arc<AtomicBool>,
    _writer: JoinHandle<()>,
    _reader: JoinHandle<()>,
}

impl NvimSession {
    /// Spawn `nvim --embed` with `cwd` as its working directory (so
    /// relative paths and the user's project-local config resolve the way
    /// they would from a terminal in that directory -- no `--clean`, the
    /// user's own `init.lua`/plugins are the point), attach the
    /// `ext_linegrid` UI at `cols`x`rows`, and start the reader/writer
    /// threads. `repaint` is called (from the reader thread) every time a
    /// redraw batch's `flush` event lands, so the UI thread knows to paint
    /// the next frame.
    pub fn spawn(
        cwd: &Path,
        cols: u16,
        rows: u16,
        repaint: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let mut child = Command::new("nvim")
            .arg("--embed")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let grid = Arc::new(Mutex::new(GridState::new(cols as usize, rows as usize)));
        let (cmd_tx, cmd_rx) = mpsc::channel::<NvimCmd>();
        let alive = Arc::new(AtomicBool::new(true));

        let writer_alive = alive.clone();
        let writer = thread::spawn(move || {
            run_writer(stdin, cmd_rx, cols, rows, writer_alive);
        });

        let grid_for_reader = grid.clone();
        let reader_alive = alive.clone();
        let reader = thread::spawn(move || {
            run_reader(stdout, grid_for_reader, repaint, reader_alive);
        });

        Ok(NvimSession {
            child,
            cmd_tx,
            grid,
            alive,
            _writer: writer,
            _reader: reader,
        })
    }

    /// Queue a command for the writer thread. If the channel is already
    /// closed (writer thread gone -- it exits the moment a write fails),
    /// flips [`Self::is_alive`] to `false` as a safety net: whatever the
    /// reason the writer's gone, a send that can't be delivered means this
    /// session can no longer talk to nvim, full stop.
    pub fn send(&self, cmd: NvimCmd) {
        if self.cmd_tx.send(cmd).is_err() {
            self.alive.store(false, Ordering::SeqCst);
        }
    }

    /// Whether the session still believes `nvim` is alive and reachable.
    /// See the struct doc for exactly what flips this to `false`.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// The shared grid state, for the renderer to lock and read each
    /// frame.
    pub fn grid(&self) -> Arc<Mutex<GridState>> {
        self.grid.clone()
    }
}

impl Drop for NvimSession {
    /// Don't leak the child process: kill and reap it. The reader/writer
    /// threads exit on their own once the pipes close (reader's decode
    /// loop errors out; writer's channel `recv` errors out once `cmd_tx`
    /// drops).
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The writer thread's body: attach the UI, then loop draining `cmd_rx`
/// into `nvim_*` requests until the channel closes (session dropped) or a
/// write fails (child gone) -- either way, the loop just ends; it never
/// panics. `alive` is flipped to `false` the moment a write fails,
/// including the initial `nvim_ui_attach` (a `spawn()` that raced an
/// instant crash should look dead immediately, not linger as "alive" until
/// the first real command).
fn run_writer(
    mut stdin: ChildStdin,
    cmd_rx: mpsc::Receiver<NvimCmd>,
    cols: u16,
    rows: u16,
    alive: Arc<AtomicBool>,
) {
    let msgid = AtomicU64::new(0);
    let attach_opts = Value::Map(vec![(Value::from("ext_linegrid"), Value::from(true))]);
    if send_request(
        &mut stdin,
        &msgid,
        "nvim_ui_attach",
        vec![Value::from(cols), Value::from(rows), attach_opts],
    )
    .is_err()
    {
        alive.store(false, Ordering::SeqCst);
        return;
    }

    for cmd in cmd_rx {
        let result = match cmd {
            NvimCmd::Input(keys) => {
                send_request(&mut stdin, &msgid, "nvim_input", vec![Value::from(keys)])
            }
            NvimCmd::Resize(cols, rows) => send_request(
                &mut stdin,
                &msgid,
                "nvim_ui_try_resize",
                vec![Value::from(cols), Value::from(rows)],
            ),
            NvimCmd::OpenFile(path, line) => send_open_file(&mut stdin, &msgid, &path, line),
            NvimCmd::Ex(command) => send_request(
                &mut stdin,
                &msgid,
                "nvim_command",
                vec![Value::from(command)],
            ),
        };
        if result.is_err() {
            alive.store(false, Ordering::SeqCst);
            break;
        }
    }
}

/// `nvim_cmd({cmd: "edit", args: [path]}, {})`, then -- if `line` is set --
/// `nvim_win_set_cursor(0, [line, 0])`. Using `nvim_cmd`'s `args` array
/// rather than building a `:edit <path>` string sidesteps Vim-command
/// escaping entirely (the alternative the spike's brief called out:
/// `fnameescape` via `nvim_call_function`). Errors from either call (e.g.
/// `E37` on a modified buffer) are swallowed -- this is a spike; the
/// session just keeps running with whatever's already open.
fn send_open_file(
    stdin: &mut ChildStdin,
    msgid: &AtomicU64,
    path: &Path,
    line: Option<u64>,
) -> io::Result<()> {
    let cmd_dict = Value::Map(vec![
        (Value::from("cmd"), Value::from("edit")),
        (
            Value::from("args"),
            Value::Array(vec![Value::from(path.to_string_lossy().into_owned())]),
        ),
    ]);
    send_request(stdin, msgid, "nvim_cmd", vec![cmd_dict, Value::Map(vec![])])?;
    if let Some(line) = line {
        send_request(
            stdin,
            msgid,
            "nvim_win_set_cursor",
            vec![
                Value::from(0),
                Value::Array(vec![Value::from(line), Value::from(0)]),
            ],
        )?;
    }
    Ok(())
}

/// Encode and write one msgpack-rpc request: `[0, msgid, method, params]`.
/// Responses are never matched up to their `msgid` -- this spike only ever
/// fires one-way commands and doesn't need their results (see the module
/// doc). The reader thread still has to drain them off the wire (they
/// arrive interleaved with `redraw` notifications), which it does by
/// simply not caring what type 1 messages contain.
fn send_request(
    stdin: &mut ChildStdin,
    msgid: &AtomicU64,
    method: &str,
    params: Vec<Value>,
) -> io::Result<()> {
    let id = msgid.fetch_add(1, Ordering::SeqCst);
    let msg = Value::Array(vec![
        Value::from(0),
        Value::from(id),
        Value::from(method),
        Value::Array(params),
    ]);
    rmpv::encode::write_value(stdin, &msg).map_err(io::Error::other)?;
    stdin.flush()
}

/// The reader thread's body: decode msgpack values off `stdout` in a loop
/// until the pipe closes (`nvim` exited -- `read_value` returns `Err` on
/// EOF, which just ends the `while let` normally; no panic), applying
/// `redraw` notifications to `grid` and calling `repaint` after every batch
/// that contains a `flush`. Flips `alive` to `false` once the loop ends,
/// however it ended.
fn run_reader(
    stdout: impl Read,
    grid: Arc<Mutex<GridState>>,
    repaint: impl Fn(),
    alive: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stdout);
    while let Ok(value) = rmpv::decode::read_value(&mut reader) {
        if let Some(events) = redraw_events(&value) {
            let flushed = events.iter().any(|e| matches!(e, RedrawEvent::Flush));
            if let Ok(mut grid) = grid.lock() {
                for event in &events {
                    grid.apply(event);
                }
            }
            if flushed {
                repaint();
            }
        }
    }
    alive.store(false, Ordering::SeqCst);
    repaint(); // wake the UI thread up so it notices `is_alive() == false` promptly.
}

/// If `message` is a `redraw` notification (`[2, "redraw", params]`), parse
/// its batch into events; `None` for anything else (requests, other
/// notifications, responses -- all silently ignored, matching the module's
/// forward-compat/spike posture).
fn redraw_events(message: &Value) -> Option<Vec<RedrawEvent>> {
    let items = message.as_array()?;
    if items.first()?.as_i64()? != 2 {
        return None;
    }
    if items.get(1)?.as_str()? != "redraw" {
        return None;
    }
    let params = items.get(2)?.as_array()?;
    Some(parse_redraw_batch(params))
}
