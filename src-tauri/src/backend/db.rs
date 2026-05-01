//! SQLite connection plumbing for the offline backend.
//!
//! - Opens the database with WAL journaling, NORMAL synchronous, and an
//!   mmap window large enough for typical workspace files.
//! - Runs schema migrations via refinery (sources under
//!   `src-tauri/migrations/`).
//! - Provides `encode_data` / `decode_data` codec helpers that wrap zstd
//!   compression around `serde_json::Value`. Penpot file-data trees are
//!   highly repetitive (tons of UUID strings, repeated keyword names) and
//!   compress to ~10–20% of their JSON size.
//!
//! Phase 2 keeps everything in one file (`workspace.sqlite`). Phase 3
//! introduces a content-addressed `media/` directory next to it.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

mod embedded {
    use refinery::embed_migrations;
    embed_migrations!("./migrations");
}

/// Open `path`, configure pragmas, and run pending migrations.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating db parent dir {}", parent.display()))?;
    }
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    )
    .with_context(|| format!("opening sqlite at {}", path.display()))?;
    apply_pragmas(&conn)?;
    embedded::migrations::runner()
        .run(&mut conn)
        .context("running schema migrations")?;
    Ok(conn)
}

/// Open an in-memory database — used by the test suite to keep parity
/// checks fast without polluting the developer's workspace.
pub fn open_in_memory() -> Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    apply_pragmas(&conn)?;
    embedded::migrations::runner().run(&mut conn)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    // WAL gives us concurrent reads while a writer holds the file —
    // important when the proxy reads `get-file` during an `update-file`.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // 256 MB memory-map: covers the entire DB on most workspaces and lets
    // SQLite skip the pager copy for read traffic.
    conn.pragma_update(None, "mmap_size", 268_435_456_i64)?;
    // Negative cache_size => kibibytes, so this is 64 MB of page cache.
    conn.pragma_update(None, "cache_size", -64_000_i64)?;
    Ok(())
}

// ───────────────────────── Codec ─────────────────────────

/// Compression level for `files.data` / snapshots. zstd 3 is a sweet
/// spot: roughly 80% of zstd-19's ratio at 5× the speed.
const ZSTD_LEVEL: i32 = 3;

/// Marker stored in `data_format`. Bumping it lets us migrate to a
/// different codec (e.g. raw JSON, msgpack) without a schema migration.
pub const DATA_FORMAT_JSON_ZSTD: &str = "json+zstd";
pub const DATA_FORMAT_JSON: &str = "json";

/// Encode a JSON value as the canonical on-disk blob format.
pub fn encode_data(value: &Value) -> Result<(Vec<u8>, &'static str)> {
    let raw = serde_json::to_vec(value).context("serializing file data to JSON")?;
    let compressed = zstd::stream::encode_all(raw.as_slice(), ZSTD_LEVEL)
        .context("compressing file data with zstd")?;
    Ok((compressed, DATA_FORMAT_JSON_ZSTD))
}

/// Decode a stored blob back into a JSON value.
pub fn decode_data(bytes: &[u8], format: &str) -> Result<Value> {
    match format {
        DATA_FORMAT_JSON_ZSTD => {
            let raw = zstd::stream::decode_all(bytes).context("decompressing zstd blob")?;
            serde_json::from_slice(&raw).context("parsing decompressed JSON")
        }
        DATA_FORMAT_JSON => serde_json::from_slice(bytes).context("parsing JSON blob"),
        other => Err(anyhow::anyhow!("unknown data_format: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_codec() {
        let v = json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "pages": [1, 2, 3],
            "objects": {"a": 1, "b": "two"}
        });
        let (blob, fmt) = encode_data(&v).unwrap();
        let back = decode_data(&blob, fmt).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn open_in_memory_runs_migrations() {
        let conn = open_in_memory().unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        // Spot-check a few schema entries from V1.
        for expected in ["teams", "projects", "files", "file_changes", "file_snapshots"] {
            assert!(
                tables.iter().any(|t| t == expected),
                "table {expected} missing from {tables:?}"
            );
        }
    }
}
