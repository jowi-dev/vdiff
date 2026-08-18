//! Embedded-Neovim: the default file viewer's pane is a real `nvim --embed`
//! instance speaking the `ext_linegrid` UI protocol over msgpack-rpc,
//! unless `--no-nvim` opts back into the legacy built-in read-only viewer.
//!
//! [`grid`] is pure -- protocol parsing and grid state, unit-tested.
//! [`session`] is impure -- process spawning, threading, and RPC framing.

pub mod grid;
pub mod session;
