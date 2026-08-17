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

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rmpv::Value;

use crate::nvim::grid::{parse_redraw_batch, GridState, RedrawEvent};

/// Reply channels for in-flight `nvim_*` requests, keyed by msgid. The
/// writer thread inserts an entry the moment it writes a [`NvimCmd::Call`]
/// request; the reader thread removes and fulfills it the moment the
/// matching `[1, msgid, error, result]` response frame arrives. Shared
/// (not owned by either thread alone) because the two threads never
/// otherwise talk to each other.
type PendingReplies = Arc<Mutex<HashMap<u64, Sender<Option<Value>>>>>;

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
    /// A request whose response the caller actually wants back --
    /// `method`/`params` as usual, `reply` is where the reader thread
    /// delivers the result (`Some(result)` on success, `None` on an RPC
    /// error). See [`NvimSession::call`] for the blocking, timeout-bounded
    /// wrapper callers actually use instead of constructing this directly.
    Call {
        method: String,
        params: Vec<Value>,
        reply: Sender<Option<Value>>,
    },
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
        let pending: PendingReplies = Arc::new(Mutex::new(HashMap::new()));

        let writer_alive = alive.clone();
        let writer_pending = pending.clone();
        let writer = thread::spawn(move || {
            run_writer(stdin, cmd_rx, cols, rows, writer_alive, writer_pending);
        });

        let grid_for_reader = grid.clone();
        let reader_alive = alive.clone();
        let reader_pending = pending.clone();
        let reader = thread::spawn(move || {
            run_reader(
                stdout,
                grid_for_reader,
                repaint,
                reader_alive,
                reader_pending,
            );
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

    /// Call `method(params)` and block the *calling* thread (not the
    /// writer/reader threads -- this is meant to be called from the UI
    /// thread, e.g. to answer "is this window at nvim's split boundary?")
    /// for up to `timeout` waiting for the response. Returns `None` on an
    /// RPC error, a timeout, or a dead session -- callers that need "is
    /// there really an answer" vs. "couldn't get one" to mean the same
    /// thing (this spike's boundary-detection queries do: no answer means
    /// "assume there's nothing to move into") can treat `None` uniformly.
    /// A wedged-but-not-dead nvim (rare, but possible) can therefore never
    /// hang the UI thread for longer than `timeout` -- this doubles as
    /// another lockup guard alongside [`Self::is_alive`].
    pub fn call(&self, method: &str, params: Vec<Value>, timeout: Duration) -> Option<Value> {
        let (reply, rx) = mpsc::channel();
        self.send(NvimCmd::Call {
            method: method.to_string(),
            params,
            reply,
        });
        rx.recv_timeout(timeout).ok().flatten()
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
    pending: PendingReplies,
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
                send_request(&mut stdin, &msgid, "nvim_input", vec![Value::from(keys)]).map(|_| ())
            }
            NvimCmd::Resize(cols, rows) => send_request(
                &mut stdin,
                &msgid,
                "nvim_ui_try_resize",
                vec![Value::from(cols), Value::from(rows)],
            )
            .map(|_| ()),
            NvimCmd::OpenFile(path, line) => send_open_file(&mut stdin, &msgid, &path, line),
            NvimCmd::Ex(command) => send_request(
                &mut stdin,
                &msgid,
                "nvim_command",
                vec![Value::from(command)],
            )
            .map(|_| ()),
            NvimCmd::Call {
                method,
                params,
                reply,
            } => match send_request(&mut stdin, &msgid, &method, params) {
                Ok(id) => {
                    if let Ok(mut pending) = pending.lock() {
                        pending.insert(id, reply);
                    }
                    Ok(())
                }
                Err(err) => {
                    let _ = reply.send(None);
                    Err(err)
                }
            },
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

/// Encode and write one msgpack-rpc request: `[0, msgid, method, params]`,
/// returning the `msgid` used. Most callers (`nvim_input`, `nvim_cmd`, ...)
/// fire-and-forget and ignore it -- responses to those just get drained off
/// the wire by the reader thread without a `pending` entry to match against
/// (see [`redraw_or_response`]). [`NvimCmd::Call`] is the one caller that
/// registers `id` in `pending` so the reader can deliver the response
/// somewhere.
fn send_request(
    stdin: &mut ChildStdin,
    msgid: &AtomicU64,
    method: &str,
    params: Vec<Value>,
) -> io::Result<u64> {
    let id = msgid.fetch_add(1, Ordering::SeqCst);
    let msg = Value::Array(vec![
        Value::from(0),
        Value::from(id),
        Value::from(method),
        Value::Array(params),
    ]);
    rmpv::encode::write_value(stdin, &msg).map_err(io::Error::other)?;
    stdin.flush()?;
    Ok(id)
}

/// The reader thread's body: decode msgpack values off `stdout` in a loop
/// until the pipe closes (`nvim` exited -- `read_value` returns `Err` on
/// EOF, which just ends the `while let` normally; no panic), applying
/// `redraw` notifications to `grid` (calling `repaint` after every batch
/// that contains a `flush`) and delivering `nvim_*` responses to whichever
/// [`NvimCmd::Call`] is waiting on them via `pending`. Flips `alive` to
/// `false` once the loop ends, however it ended, and drains `pending` so
/// any [`NvimSession::call`] still blocked gets an immediate `None` instead
/// of waiting out its full timeout.
fn run_reader(
    stdout: impl Read,
    grid: Arc<Mutex<GridState>>,
    repaint: impl Fn(),
    alive: Arc<AtomicBool>,
    pending: PendingReplies,
) {
    let mut reader = BufReader::new(stdout);
    while let Ok(value) = rmpv::decode::read_value(&mut reader) {
        let Some(items) = value.as_array() else {
            continue;
        };
        match items.first().and_then(rmpv::Value::as_i64) {
            Some(1) => deliver_response(items, &pending),
            Some(2) => {
                if let Some(events) = parse_redraw_notification(items) {
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
            _ => {} // a request from nvim to us, or something malformed -- ignored either way.
        }
    }
    alive.store(false, Ordering::SeqCst);
    if let Ok(mut pending) = pending.lock() {
        pending.clear(); // dropping the reply senders wakes any blocked `call()` immediately.
    }
    repaint(); // wake the UI thread up so it notices `is_alive() == false` promptly.
}

/// If `items` is a `redraw` notification's body (`["redraw", params]`,
/// i.e. everything after the `[2, ...]` type tag already matched by the
/// caller), parse its batch into events; `None` for anything else
/// (silently ignored, matching the module's forward-compat/spike posture).
fn parse_redraw_notification(items: &[Value]) -> Option<Vec<RedrawEvent>> {
    if items.get(1)?.as_str()? != "redraw" {
        return None;
    }
    let params = items.get(2)?.as_array()?;
    Some(parse_redraw_batch(params))
}

/// Deliver a `[1, msgid, error, result]` response frame to whichever
/// [`NvimCmd::Call`] registered `msgid` in `pending` (removing the entry --
/// each request gets exactly one response). A response for an id nobody's
/// waiting on (already timed out, or this session never issued it) is
/// silently dropped. `error` being non-nil is treated the same as a
/// missing result: `None`, not the error payload -- this spike's only
/// caller ([`crate::ui::nvim_pane::NvimPane::at_boundary`] and `--nvim-cmd`
/// warnings) only needs "did this work", not the error's shape.
fn deliver_response(items: &[Value], pending: &PendingReplies) {
    let Some(msgid) = items.get(1).and_then(rmpv::Value::as_u64) else {
        return;
    };
    let Some(reply) = pending.lock().ok().and_then(|mut map| map.remove(&msgid)) else {
        return;
    };
    let is_error = !matches!(items.get(2), Some(Value::Nil) | None);
    let result = if is_error {
        None
    } else {
        items.get(3).cloned()
    };
    let _ = reply.send(result);
}
