//! IPC client for the DEX runtime.
//!
//! Knows the wire format and nothing else: it opens a socket, writes request
//! frames, reads response frames, and hands the caller a stream of events. It
//! has no opinion about what a session is doing and performs no repository
//! operation of its own.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
