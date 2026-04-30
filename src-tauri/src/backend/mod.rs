//! Embedded Penpot backend.
//!
//! Replaces the JVM-based Penpot backend for single-user offline workflows.
//! See `docs/offline-backend.md` for the architectural overview. The pieces:
//!
//! - `transit`: Cognitect Transit (JSON, non-verbose) reader/writer.
//!   Required for `.penpot` archives (binfile-v3) and as a fallback wire
//!   format. The RPC dispatcher itself prefers plain JSON via
//!   `enable-transit-readable-response`.
//! - `model`: Penpot data types backed by `serde_json::Value` for
//!   round-trip fidelity (Penpot adds shape fields with every release).
//! - `binfile`: `.penpot` ZIP container handling (binfile-v3).
//! - `changes`: pure-functional change applier — the in-process equivalent
//!   of Penpot's `process-change` multimethod.
//! - `store`: in-memory file/project/team store.
//! - `rpc`: HTTP RPC dispatcher.
//! - `flags`: `penpotFlags` injection for offline mode.

pub mod binfile;
pub mod changes;
pub mod flags;
pub mod model;
pub mod rpc;
pub mod store;
pub mod transit;
