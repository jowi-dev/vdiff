//! Embedded-Neovim spike: `--nvim` replaces the built-in read-only file
//! viewer's pane with a real `nvim --embed` instance speaking the
//! `ext_linegrid` UI protocol over msgpack-rpc.
//!
//! [`grid`] is pure -- protocol parsing and grid state, unit-tested.
//! [`session`] is impure -- process spawning, threading, and RPC framing.

pub mod grid;
pub mod session;
