//! Local media storage for offline mode.
//!
//! Each uploaded asset is keyed by a freshly generated UUID (the
//! "storage object id") matching what Penpot's file.data.media[*].id
//! references. Bytes live on disk at
//! `<root>/<id[0..2]>/<id>` — the two-character shard keeps any single
//! directory under a few hundred entries even with 100k+ assets.
//!
//! SHA-256 is computed and stored alongside the row so a future dedup
//! pass can collapse identical bytes onto a single physical file.
//! Today every upload writes a fresh file; the cost is acceptable
//! because Penpot files rarely re-upload the same image.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub struct MediaMetadata {
    pub id: Uuid,
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct StoredMedia {
    pub id: Uuid,
    pub sha256: String,
    pub size: u64,
    pub mime_type: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct StoreMediaRequest<'a> {
    pub bytes: &'a [u8],
    pub mime_type: &'a str,
    pub explicit_id: Option<Uuid>,
}

/// Compute the on-disk path for a storage-object UUID under `root`.
/// Always uses the first two hex chars of the UUID as a shard.
pub fn path_for(root: &Path, id: Uuid) -> PathBuf {
    let hex = format!("{:032x}", id.as_u128());
    root.join(&hex[..2]).join(&hex)
}

/// Write a media blob to disk + insert the metadata row in one shot.
/// Returns the persisted descriptor. If `explicit_id` is provided, that
/// id is reused (used during `.penpot` import to keep storage IDs stable
/// across export/import).
pub fn store_media(
    conn: &mut Connection,
    media_root: &Path,
    req: StoreMediaRequest<'_>,
) -> Result<StoredMedia> {
    let id = req.explicit_id.unwrap_or_else(Uuid::new_v4);
    let sha = sha256_hex(req.bytes);
    let path = path_for(media_root, id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating media shard {}", parent.display()))?;
    }
    if !path.exists() {
        std::fs::write(&path, req.bytes)
            .with_context(|| format!("writing media to {}", path.display()))?;
    }
    let (width, height) = probe_dimensions(req.bytes);
    let now = Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO media (id, sha256, size, mime_type, width, height, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(id) DO UPDATE SET \
           sha256 = excluded.sha256, \
           size = excluded.size, \
           mime_type = excluded.mime_type, \
           width = COALESCE(excluded.width, width), \
           height = COALESCE(excluded.height, height)",
        params![
            id.as_bytes(),
            sha,
            req.bytes.len() as i64,
            req.mime_type,
            width.map(|w| w as i64),
            height.map(|h| h as i64),
            now,
        ],
    )
    .context("inserting media metadata")?;
    Ok(StoredMedia {
        id,
        sha256: sha256_hex(req.bytes),
        size: req.bytes.len() as u64,
        mime_type: req.mime_type.to_string(),
        width,
        height,
    })
}

/// Look up media metadata by id without touching the disk.
pub fn get_metadata(conn: &Connection, id: Uuid) -> Result<Option<StoredMedia>> {
    conn.query_row(
        "SELECT id, sha256, size, mime_type, width, height FROM media WHERE id = ?1",
        params![id.as_bytes()],
        |row| {
            Ok(StoredMedia {
                id: bytes_to_uuid(row.get::<_, Vec<u8>>(0)?),
                sha256: row.get(1)?,
                size: row.get::<_, i64>(2)? as u64,
                mime_type: row.get(3)?,
                width: row.get::<_, Option<i64>>(4)?.map(|v| v as u32),
                height: row.get::<_, Option<i64>>(5)?.map(|v| v as u32),
            })
        },
    )
    .optional()
    .context("querying media metadata")
}

/// Read the bytes for a stored media id.
pub fn read_bytes(media_root: &Path, id: Uuid) -> Result<Vec<u8>> {
    let path = path_for(media_root, id);
    std::fs::read(&path).with_context(|| format!("reading media file {}", path.display()))
}

/// Insert a per-file media reference (file.data.media[asset_id] →
/// storage-object). Idempotent on `(file_id, asset_id)`.
pub fn insert_file_media_ref(
    conn: &Connection,
    file_id: Uuid,
    media_id: Uuid,
    asset_id: Uuid,
    name: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO file_media_refs (file_id, media_id, asset_id, name) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(file_id, asset_id) DO UPDATE SET \
           media_id = excluded.media_id, \
           name = excluded.name",
        params![
            file_id.as_bytes(),
            media_id.as_bytes(),
            asset_id.as_bytes(),
            name,
        ],
    )?;
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Best-effort image-dimension probe. Returns `(None, None)` for
/// non-image MIME types or formats `imagesize` doesn't recognise.
pub fn probe_dimensions(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    match imagesize::blob_size(bytes) {
        Ok(size) => (Some(size.width as u32), Some(size.height as u32)),
        Err(_) => (None, None),
    }
}

fn bytes_to_uuid(bytes: Vec<u8>) -> Uuid {
    Uuid::from_slice(&bytes).unwrap_or_else(|_| Uuid::nil())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::db;

    fn fresh_conn() -> Connection {
        db::open_in_memory().unwrap()
    }

    #[test]
    fn path_for_shards_by_first_two_chars() {
        let id = Uuid::parse_str("12345678-1234-1234-1234-123456789abc").unwrap();
        let p = path_for(Path::new("/media"), id);
        assert!(p.starts_with("/media/12"));
        assert!(p.to_string_lossy().contains("123456781234123412341234"));
    }

    #[test]
    fn round_trip_store_and_read() {
        let dir = tempdir();
        let mut conn = fresh_conn();
        let req = StoreMediaRequest {
            bytes: b"hello world",
            mime_type: "application/octet-stream",
            explicit_id: None,
        };
        let stored = store_media(&mut conn, &dir, req).unwrap();
        assert_eq!(stored.size, 11);
        let back = read_bytes(&dir, stored.id).unwrap();
        assert_eq!(back, b"hello world");
        let meta = get_metadata(&conn, stored.id).unwrap().unwrap();
        assert_eq!(meta.size, 11);
        assert_eq!(meta.sha256, sha256_hex(b"hello world"));
    }

    #[test]
    fn probes_png_dimensions() {
        // Smallest valid PNG (1×1 transparent pixel) — RFC sample.
        let png: [u8; 67] = [
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x62, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let (w, h) = probe_dimensions(&png);
        assert_eq!(w, Some(1));
        assert_eq!(h, Some(1));
    }

    #[test]
    fn explicit_id_is_preserved() {
        let dir = tempdir();
        let mut conn = fresh_conn();
        let id = Uuid::new_v4();
        let stored = store_media(
            &mut conn,
            &dir,
            StoreMediaRequest {
                bytes: b"x",
                mime_type: "image/png",
                explicit_id: Some(id),
            },
        )
        .unwrap();
        assert_eq!(stored.id, id);
        assert!(read_bytes(&dir, id).is_ok());
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("penpot-media-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
