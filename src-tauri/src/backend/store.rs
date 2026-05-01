//! Persistent file/project/team store for offline mode.
//!
//! Two backends behind one API:
//! - `Backend::Memory` — Phase 1's RAM-only store. Used by tests and as a
//!   fallback when no DB path is configured.
//! - `Backend::Sqlite` — WAL-mode SQLite via [`super::db`]. The default
//!   in production. Survives app restarts; serializes writes through a
//!   single connection mutex (one writer at a time, plus WAL readers).
//!
//! The public methods on `Store` are identical between the two backends,
//! so RPC code (`backend::rpc`) and Tauri command code (`commands.rs`)
//! never branch on storage type.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde_json::Value;
use uuid::Uuid;

use super::changes::apply_changes;
use super::db;
use super::media::{self, StoreMediaRequest, StoredMedia};
use super::model::{
    self, File, Project, Team, FILE_DATA_VERSION, LOCAL_PROJECT_ID, LOCAL_TEAM_ID,
};

/// Auto-snapshot every Nth revn — matches Penpot's
/// `PENPOT_AUTO_FILE_SNAPSHOT_EVERY` default.
const AUTO_SNAPSHOT_EVERY: i64 = 10;

/// Keep at most this many auto-snapshots per file.
const AUTO_SNAPSHOT_KEEP: usize = 10;

/// Cap on per-file change-log entries kept in the in-memory backend.
/// SQLite has no equivalent cap — `file_changes` rows are kept until the
/// file is deleted.
const MEMORY_HISTORY_CAP: usize = 200;

#[derive(Clone, Debug)]
pub struct ChangeRecord {
    pub revn: i64,
    pub session_id: Uuid,
    pub changes: Value,
    pub undo: Value,
    pub at: DateTime<Utc>,
}

// ───────────────────────── Update flow ─────────────────────────

#[derive(Debug, Clone)]
pub struct UpdateRequest {
    pub file_id: Uuid,
    pub client_revn: i64,
    pub session_id: Uuid,
    pub changes: Vec<Value>,
}

#[derive(Debug, Clone)]
pub enum UpdateOutcome {
    Applied { new_revn: i64, vern: i64 },
    Lagged { server_revn: i64, missed: Vec<Value> },
    NotFound,
    Error(String),
}

// ───────────────────────── Public Store ─────────────────────────

#[derive(Clone)]
pub struct Store {
    backend: Arc<Backend>,
    /// Directory where media bytes live for the SQLite backend. Memory
    /// backend ignores this and keeps blobs in RAM.
    media_root: Option<PathBuf>,
}

enum Backend {
    Memory(RwLock<MemoryState>),
    Sqlite(Mutex<Connection>),
}

impl Store {
    /// Build an in-memory store seeded with the local team + Drafts project.
    pub fn in_memory() -> Self {
        let state = MemoryState::seeded();
        Self {
            backend: Arc::new(Backend::Memory(RwLock::new(state))),
            media_root: None,
        }
    }

    /// Backwards-compatible alias used by Phase 1 tests.
    pub fn seeded() -> Self {
        Self::in_memory()
    }

    /// Open or create a SQLite-backed store at `path`. Media files are
    /// written next to the database under `<path-parent>/media/`.
    pub fn open_sqlite(path: &std::path::Path) -> Result<Self> {
        let conn = db::open(path).context("opening sqlite store")?;
        let media_root = path
            .parent()
            .map(|p| p.join("media"))
            .unwrap_or_else(|| PathBuf::from("media"));
        std::fs::create_dir_all(&media_root)
            .with_context(|| format!("creating media root {}", media_root.display()))?;
        Self::from_connection(conn, Some(media_root))
    }

    /// In-memory SQLite (test-only). Same migrations, no on-disk file.
    /// Media is written to a fresh temp directory.
    #[cfg(test)]
    pub fn in_memory_sqlite() -> Result<Self> {
        let conn = db::open_in_memory()?;
        let dir = std::env::temp_dir()
            .join(format!("penpot-store-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        Self::from_connection(conn, Some(dir))
    }

    fn from_connection(conn: Connection, media_root: Option<PathBuf>) -> Result<Self> {
        let store = Self {
            backend: Arc::new(Backend::Sqlite(Mutex::new(conn))),
            media_root,
        };
        store.seed_defaults_if_empty()?;
        Ok(store)
    }

    fn seed_defaults_if_empty(&self) -> Result<()> {
        match &*self.backend {
            Backend::Sqlite(conn) => {
                let mut conn = conn.lock().unwrap();
                let team_count: i64 = conn
                    .query_row("SELECT COUNT(*) FROM teams", [], |r| r.get(0))
                    .unwrap_or(0);
                if team_count == 0 {
                    let team = Team::local(LOCAL_TEAM_ID);
                    let project = Project::local(LOCAL_PROJECT_ID, LOCAL_TEAM_ID);
                    let tx = conn.transaction()?;
                    insert_team(&tx, &team)?;
                    insert_project(&tx, &project)?;
                    tx.commit()?;
                }
                Ok(())
            }
            Backend::Memory(_) => Ok(()), // already seeded by `in_memory()`
        }
    }

    // ──────────────── Reads ────────────────

    pub fn list_teams(&self) -> Vec<Team> {
        match &*self.backend {
            Backend::Memory(s) => s.read().unwrap().teams.values().cloned().collect(),
            Backend::Sqlite(conn) => {
                let conn = conn.lock().unwrap();
                conn.prepare("SELECT id, name, is_default, features, created_at, modified_at FROM teams")
                    .ok()
                    .and_then(|mut stmt| {
                        stmt.query_map([], row_to_team)
                            .ok()
                            .map(|it| it.filter_map(Result::ok).collect())
                    })
                    .unwrap_or_default()
            }
        }
    }

    pub fn get_team(&self, id: Uuid) -> Option<Team> {
        match &*self.backend {
            Backend::Memory(s) => s.read().unwrap().teams.get(&id).cloned(),
            Backend::Sqlite(conn) => {
                let conn = conn.lock().unwrap();
                conn.query_row(
                    "SELECT id, name, is_default, features, created_at, modified_at \
                     FROM teams WHERE id = ?1",
                    params![id.as_bytes()],
                    row_to_team,
                )
                .optional()
                .ok()
                .flatten()
            }
        }
    }

    pub fn list_projects(&self, team_id: Uuid) -> Vec<Project> {
        match &*self.backend {
            Backend::Memory(s) => s
                .read()
                .unwrap()
                .projects
                .values()
                .filter(|p| p.team_id == team_id)
                .cloned()
                .collect(),
            Backend::Sqlite(conn) => {
                let conn = conn.lock().unwrap();
                conn.prepare(
                    "SELECT id, team_id, name, is_default, is_pinned, created_at, modified_at \
                     FROM projects WHERE team_id = ?1 AND deleted_at IS NULL",
                )
                .ok()
                .and_then(|mut stmt| {
                    stmt.query_map(params![team_id.as_bytes()], row_to_project)
                        .ok()
                        .map(|it| it.filter_map(Result::ok).collect())
                })
                .unwrap_or_default()
            }
        }
    }

    pub fn list_all_projects(&self) -> Vec<Project> {
        match &*self.backend {
            Backend::Memory(s) => s.read().unwrap().projects.values().cloned().collect(),
            Backend::Sqlite(conn) => {
                let conn = conn.lock().unwrap();
                conn.prepare(
                    "SELECT id, team_id, name, is_default, is_pinned, created_at, modified_at \
                     FROM projects WHERE deleted_at IS NULL",
                )
                .ok()
                .and_then(|mut stmt| {
                    stmt.query_map([], row_to_project)
                        .ok()
                        .map(|it| it.filter_map(Result::ok).collect())
                })
                .unwrap_or_default()
            }
        }
    }

    pub fn list_project_files(&self, project_id: Uuid) -> Vec<File> {
        match &*self.backend {
            Backend::Memory(s) => s
                .read()
                .unwrap()
                .files
                .values()
                .filter(|f| f.project_id == project_id)
                .cloned()
                .collect(),
            Backend::Sqlite(conn) => {
                let conn = conn.lock().unwrap();
                conn.prepare(file_select_sql_with_filter("project_id = ?1 AND deleted_at IS NULL"))
                    .ok()
                    .and_then(|mut stmt| {
                        stmt.query_map(params![project_id.as_bytes()], row_to_file)
                            .ok()
                            .map(|it| it.filter_map(Result::ok).collect())
                    })
                    .unwrap_or_default()
            }
        }
    }

    pub fn get_file(&self, id: Uuid) -> Option<File> {
        match &*self.backend {
            Backend::Memory(s) => s.read().unwrap().files.get(&id).cloned(),
            Backend::Sqlite(conn) => {
                let conn = conn.lock().unwrap();
                conn.query_row(
                    file_select_sql_with_filter("id = ?1 AND deleted_at IS NULL"),
                    params![id.as_bytes()],
                    row_to_file,
                )
                .optional()
                .ok()
                .flatten()
            }
        }
    }

    pub fn put_file(&self, file: File) {
        match &*self.backend {
            Backend::Memory(s) => {
                s.write().unwrap().files.insert(file.id, file);
            }
            Backend::Sqlite(conn) => {
                let mut conn = conn.lock().unwrap();
                let _ = (|| -> Result<()> {
                    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    upsert_file(&tx, &file)?;
                    tx.commit()?;
                    Ok(())
                })();
            }
        }
    }

    pub fn delete_file(&self, id: Uuid) -> Option<File> {
        match &*self.backend {
            Backend::Memory(s) => {
                let mut state = s.write().unwrap();
                state.change_log.remove(&id);
                state.files.remove(&id)
            }
            Backend::Sqlite(conn) => {
                let mut conn = conn.lock().unwrap();
                let now = Utc::now().timestamp_millis();
                // Soft delete — same semantics as the in-memory backend
                // (the row vanishes from list/get) but keeps related
                // file_changes / file_snapshots intact in case we add an
                // "undo delete" later.
                let prev = conn
                    .query_row(
                        file_select_sql_with_filter("id = ?1"),
                        params![id.as_bytes()],
                        row_to_file,
                    )
                    .optional()
                    .ok()
                    .flatten();
                let _ = conn.execute(
                    "UPDATE files SET deleted_at = ?1 WHERE id = ?2",
                    params![now, id.as_bytes()],
                );
                prev
            }
        }
    }

    /// Mutate a file's metadata fields (name/is-shared/etc.) atomically.
    /// For data-tree mutations driven by changes, prefer [`Self::apply_update`].
    pub fn with_file_mut<F, R>(&self, id: Uuid, f: F) -> Option<R>
    where
        F: FnOnce(&mut File) -> R,
    {
        match &*self.backend {
            Backend::Memory(s) => {
                let mut state = s.write().unwrap();
                state.files.get_mut(&id).map(|file| {
                    let r = f(file);
                    file.modified_at = Utc::now();
                    r
                })
            }
            Backend::Sqlite(conn) => {
                let mut conn = conn.lock().unwrap();
                let mut file = conn
                    .query_row(
                        file_select_sql_with_filter("id = ?1 AND deleted_at IS NULL"),
                        params![id.as_bytes()],
                        row_to_file,
                    )
                    .optional()
                    .ok()
                    .flatten()?;
                let r = f(&mut file);
                file.modified_at = Utc::now();
                let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
                    Ok(tx) => tx,
                    Err(_) => return Some(r),
                };
                if upsert_file(&tx, &file).is_ok() {
                    let _ = tx.commit();
                }
                Some(r)
            }
        }
    }

    /// Atomic apply-update flow: revn check → apply changes → bump revn →
    /// record the change-log row → optional auto-snapshot. Single
    /// transaction in the SQLite backend so a crash mid-update can never
    /// leave the file_changes log out of sync with `files.revn`.
    pub fn apply_update(&self, req: UpdateRequest) -> UpdateOutcome {
        match &*self.backend {
            Backend::Memory(s) => self.apply_update_memory(s, req),
            Backend::Sqlite(conn) => self.apply_update_sqlite(conn, req),
        }
    }

    fn apply_update_memory(
        &self,
        state: &RwLock<MemoryState>,
        req: UpdateRequest,
    ) -> UpdateOutcome {
        let mut state = state.write().unwrap();
        let MemoryState {
            files, change_log, ..
        } = &mut *state;
        let file = match files.get_mut(&req.file_id) {
            Some(f) => f,
            None => return UpdateOutcome::NotFound,
        };
        if file.revn != req.client_revn {
            let server_revn = file.revn;
            let missed: Vec<Value> = change_log
                .get(&req.file_id)
                .into_iter()
                .flatten()
                .filter(|r| r.revn > req.client_revn)
                .flat_map(|r| match &r.changes {
                    Value::Array(arr) => arr.clone(),
                    _ => vec![],
                })
                .collect();
            return UpdateOutcome::Lagged { server_revn, missed };
        }
        let undo = match apply_changes(&mut file.data, &req.changes) {
            Ok(u) => u,
            Err(e) => return UpdateOutcome::Error(e.to_string()),
        };
        file.revn += 1;
        file.modified_at = Utc::now();
        let new_revn = file.revn;
        let vern = file.vern;
        let log = change_log.entry(req.file_id).or_default();
        log.push(ChangeRecord {
            revn: new_revn,
            session_id: req.session_id,
            changes: Value::Array(req.changes),
            undo: Value::Array(undo),
            at: Utc::now(),
        });
        if log.len() > MEMORY_HISTORY_CAP {
            let drop = log.len() - MEMORY_HISTORY_CAP;
            log.drain(0..drop);
        }
        UpdateOutcome::Applied { new_revn, vern }
    }

    fn apply_update_sqlite(
        &self,
        conn: &Mutex<Connection>,
        req: UpdateRequest,
    ) -> UpdateOutcome {
        let mut conn = conn.lock().unwrap();
        match try_apply_update_sqlite(&mut conn, &req) {
            Ok(o) => o,
            Err(e) => UpdateOutcome::Error(e.to_string()),
        }
    }

    pub fn record_changes(&self, file_id: Uuid, record: ChangeRecord) {
        // Retained for compatibility with non-update flows (rare). The
        // primary update path goes through `apply_update`.
        match &*self.backend {
            Backend::Memory(s) => {
                let mut state = s.write().unwrap();
                let log = state.change_log.entry(file_id).or_default();
                log.push(record);
                if log.len() > MEMORY_HISTORY_CAP {
                    let drop = log.len() - MEMORY_HISTORY_CAP;
                    log.drain(0..drop);
                }
            }
            Backend::Sqlite(conn) => {
                let conn = conn.lock().unwrap();
                let _ = conn.execute(
                    "INSERT INTO file_changes \
                     (file_id, revn, session_id, commit_id, label, changes, undo_changes, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        file_id.as_bytes(),
                        record.revn,
                        record.session_id.as_bytes(),
                        Uuid::new_v4().as_bytes(),
                        Option::<String>::None,
                        serde_json::to_vec(&record.changes).unwrap_or_default(),
                        serde_json::to_vec(&record.undo).unwrap_or_default(),
                        record.at.timestamp_millis(),
                    ],
                );
            }
        }
    }

    pub fn change_log(&self, file_id: Uuid) -> Vec<ChangeRecord> {
        match &*self.backend {
            Backend::Memory(s) => s
                .read()
                .unwrap()
                .change_log
                .get(&file_id)
                .cloned()
                .unwrap_or_default(),
            Backend::Sqlite(conn) => {
                let conn = conn.lock().unwrap();
                conn.prepare(
                    "SELECT revn, session_id, changes, undo_changes, created_at \
                     FROM file_changes WHERE file_id = ?1 ORDER BY revn ASC",
                )
                .ok()
                .and_then(|mut stmt| {
                    stmt.query_map(params![file_id.as_bytes()], row_to_change_record)
                        .ok()
                        .map(|it| it.filter_map(Result::ok).collect())
                })
                .unwrap_or_default()
            }
        }
    }

    /// Reset the entire store. In-memory only — the SQLite backend keeps
    /// its data across runs and requires explicit migrations to clear.
    #[cfg(test)]
    pub fn reset(&self) {
        if let Backend::Memory(s) = &*self.backend {
            *s.write().unwrap() = MemoryState::seeded();
        }
    }

    // ──────────────── Media ────────────────

    /// Persist a binary asset. `explicit_id` lets the caller fix the
    /// storage-object id (used by `.penpot` import to keep ids stable
    /// across export/import cycles); pass `None` for a fresh upload.
    pub fn store_media(
        &self,
        bytes: &[u8],
        mime_type: &str,
        explicit_id: Option<Uuid>,
    ) -> Result<StoredMedia> {
        match &*self.backend {
            Backend::Memory(state) => {
                let id = explicit_id.unwrap_or_else(Uuid::new_v4);
                let (width, height) = media::probe_dimensions(bytes);
                state
                    .write()
                    .unwrap()
                    .media
                    .insert(id, (mime_type.to_string(), bytes.to_vec()));
                Ok(StoredMedia {
                    id,
                    sha256: media::sha256_hex(bytes),
                    size: bytes.len() as u64,
                    mime_type: mime_type.to_string(),
                    width,
                    height,
                })
            }
            Backend::Sqlite(conn) => {
                let root = self
                    .media_root
                    .as_ref()
                    .context("sqlite store has no media root")?;
                let mut conn = conn.lock().unwrap();
                media::store_media(
                    &mut conn,
                    root,
                    StoreMediaRequest {
                        bytes,
                        mime_type,
                        explicit_id,
                    },
                )
            }
        }
    }

    /// Read media bytes by storage-object id.
    pub fn read_media(&self, id: Uuid) -> Option<Vec<u8>> {
        match &*self.backend {
            Backend::Memory(state) => state
                .read()
                .unwrap()
                .media
                .get(&id)
                .map(|(_, bytes)| bytes.clone()),
            Backend::Sqlite(_) => {
                let root = self.media_root.as_ref()?;
                media::read_bytes(root, id).ok()
            }
        }
    }

    /// Look up media metadata without reading bytes.
    pub fn media_metadata(&self, id: Uuid) -> Option<StoredMedia> {
        match &*self.backend {
            Backend::Memory(state) => {
                let st = state.read().unwrap();
                let (mime, bytes) = st.media.get(&id)?;
                let (w, h) = media::probe_dimensions(bytes);
                Some(StoredMedia {
                    id,
                    sha256: media::sha256_hex(bytes),
                    size: bytes.len() as u64,
                    mime_type: mime.clone(),
                    width: w,
                    height: h,
                })
            }
            Backend::Sqlite(conn) => {
                let conn = conn.lock().unwrap();
                media::get_metadata(&conn, id).ok().flatten()
            }
        }
    }

    /// Record a per-file media reference (file.data.media[asset_id] →
    /// storage object). Idempotent on `(file_id, asset_id)`.
    pub fn link_file_media(
        &self,
        file_id: Uuid,
        media_id: Uuid,
        asset_id: Uuid,
        name: &str,
    ) -> Result<()> {
        match &*self.backend {
            Backend::Memory(_) => Ok(()),
            Backend::Sqlite(conn) => {
                let conn = conn.lock().unwrap();
                media::insert_file_media_ref(&conn, file_id, media_id, asset_id, name)
            }
        }
    }

    /// Best-effort resolver for `media_provider` callbacks in
    /// `binfile::export_to_bytes` — returns the on-disk bytes for a
    /// storage-object id, or None if missing.
    pub fn media_provider(&self) -> impl Fn(&str) -> Option<Vec<u8>> + '_ {
        move |id_str: &str| {
            let id = Uuid::parse_str(id_str).ok()?;
            self.read_media(id)
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::in_memory()
    }
}

// ───────────────────────── Memory state ─────────────────────────

#[derive(Default)]
struct MemoryState {
    teams: HashMap<Uuid, Team>,
    projects: HashMap<Uuid, Project>,
    files: HashMap<Uuid, File>,
    change_log: HashMap<Uuid, Vec<ChangeRecord>>,
    /// In-memory media: id → (mime_type, bytes). Width/height aren't
    /// stored separately since the in-memory backend re-probes on read
    /// when needed (rare; tests only).
    media: HashMap<Uuid, (String, Vec<u8>)>,
}

impl MemoryState {
    fn seeded() -> Self {
        let mut s = Self::default();
        s.teams.insert(LOCAL_TEAM_ID, Team::local(LOCAL_TEAM_ID));
        s.projects
            .insert(LOCAL_PROJECT_ID, Project::local(LOCAL_PROJECT_ID, LOCAL_TEAM_ID));
        s
    }
}

// ───────────────────────── SQLite helpers ─────────────────────────

fn file_select_sql_with_filter(filter: &str) -> &str {
    // Rusqlite needs &str / &'static str — we side-step that by hard-coding
    // each filter location's full SQL string. Returns a leaked &'static str
    // via a memoized table.
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().unwrap();
    let key = filter.to_string();
    if let Some(&s) = map.get(&key) {
        return s;
    }
    let sql = format!(
        "SELECT id, project_id, name, revn, vern, version, is_shared, features, \
         data, data_format, created_at, modified_at \
         FROM files WHERE {filter}"
    );
    let leaked: &'static str = Box::leak(sql.into_boxed_str());
    map.insert(key, leaked);
    leaked
}

fn try_apply_update_sqlite(
    conn: &mut Connection,
    req: &UpdateRequest,
) -> Result<UpdateOutcome> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row = tx
        .query_row(
            file_select_sql_with_filter("id = ?1 AND deleted_at IS NULL"),
            params![req.file_id.as_bytes()],
            row_to_file,
        )
        .optional()?;
    let mut file = match row {
        Some(f) => f,
        None => return Ok(UpdateOutcome::NotFound),
    };
    if file.revn != req.client_revn {
        // Replay missed change vectors back to the client.
        let server_revn = file.revn;
        let mut stmt = tx.prepare(
            "SELECT changes FROM file_changes \
             WHERE file_id = ?1 AND revn > ?2 ORDER BY revn ASC",
        )?;
        let mut missed = Vec::new();
        let mut rows = stmt.query(params![req.file_id.as_bytes(), req.client_revn])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            if let Ok(Value::Array(arr)) = serde_json::from_slice::<Value>(&bytes) {
                missed.extend(arr);
            }
        }
        return Ok(UpdateOutcome::Lagged { server_revn, missed });
    }
    let undo = match apply_changes(&mut file.data, &req.changes) {
        Ok(u) => u,
        Err(e) => return Ok(UpdateOutcome::Error(e.to_string())),
    };
    file.revn += 1;
    file.modified_at = Utc::now();
    let new_revn = file.revn;
    let vern = file.vern;

    upsert_file(&tx, &file)?;

    let changes_blob = serde_json::to_vec(&Value::Array(req.changes.clone()))
        .context("encoding change vector")?;
    let undo_blob = serde_json::to_vec(&Value::Array(undo)).context("encoding undo vector")?;
    let label = label_for_changes(&req.changes);
    tx.execute(
        "INSERT INTO file_changes \
         (file_id, revn, session_id, commit_id, label, changes, undo_changes, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            req.file_id.as_bytes(),
            new_revn,
            req.session_id.as_bytes(),
            Uuid::new_v4().as_bytes(),
            label,
            changes_blob,
            undo_blob,
            Utc::now().timestamp_millis(),
        ],
    )?;

    if new_revn % AUTO_SNAPSHOT_EVERY == 0 {
        write_auto_snapshot(&tx, req.file_id, new_revn, &file.data)?;
        prune_auto_snapshots(&tx, req.file_id, AUTO_SNAPSHOT_KEEP)?;
    }

    tx.commit()?;
    Ok(UpdateOutcome::Applied { new_revn, vern })
}

fn upsert_file(tx: &Transaction<'_>, file: &File) -> Result<()> {
    let (data_blob, data_format) = db::encode_data(&file.data)?;
    tx.execute(
        "INSERT INTO files \
         (id, project_id, name, revn, vern, version, is_shared, features, \
          data, data_format, created_at, modified_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
         ON CONFLICT(id) DO UPDATE SET \
          project_id = excluded.project_id, \
          name = excluded.name, \
          revn = excluded.revn, \
          vern = excluded.vern, \
          version = excluded.version, \
          is_shared = excluded.is_shared, \
          features = excluded.features, \
          data = excluded.data, \
          data_format = excluded.data_format, \
          modified_at = excluded.modified_at, \
          deleted_at = NULL",
        params![
            file.id.as_bytes(),
            file.project_id.as_bytes(),
            file.name,
            file.revn,
            file.vern,
            file.version,
            file.is_shared as i32,
            serde_json::to_string(&file.features).unwrap_or_else(|_| "[]".into()),
            data_blob,
            data_format,
            file.created_at.timestamp_millis(),
            file.modified_at.timestamp_millis(),
        ],
    )?;
    Ok(())
}

fn insert_team(tx: &Transaction<'_>, team: &Team) -> Result<()> {
    tx.execute(
        "INSERT INTO teams (id, name, is_default, features, created_at, modified_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(id) DO NOTHING",
        params![
            team.id.as_bytes(),
            team.name,
            team.is_default as i32,
            serde_json::to_string(&team.features).unwrap_or_else(|_| "[]".into()),
            team.created_at.timestamp_millis(),
            team.modified_at.timestamp_millis(),
        ],
    )?;
    Ok(())
}

fn insert_project(tx: &Transaction<'_>, project: &Project) -> Result<()> {
    tx.execute(
        "INSERT INTO projects \
         (id, team_id, name, is_pinned, is_default, created_at, modified_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(id) DO NOTHING",
        params![
            project.id.as_bytes(),
            project.team_id.as_bytes(),
            project.name,
            project.is_pinned as i32,
            project.is_default as i32,
            project.created_at.timestamp_millis(),
            project.modified_at.timestamp_millis(),
        ],
    )?;
    Ok(())
}

fn write_auto_snapshot(
    tx: &Transaction<'_>,
    file_id: Uuid,
    revn: i64,
    data: &Value,
) -> Result<()> {
    let (blob, fmt) = db::encode_data(data)?;
    tx.execute(
        "INSERT INTO file_snapshots \
         (id, file_id, revn, label, data, data_format, created_at) \
         VALUES (?1, ?2, ?3, 'auto', ?4, ?5, ?6)",
        params![
            Uuid::new_v4().as_bytes(),
            file_id.as_bytes(),
            revn,
            blob,
            fmt,
            Utc::now().timestamp_millis(),
        ],
    )?;
    Ok(())
}

fn prune_auto_snapshots(tx: &Transaction<'_>, file_id: Uuid, keep: usize) -> Result<()> {
    tx.execute(
        "DELETE FROM file_snapshots \
         WHERE file_id = ?1 AND label = 'auto' AND id NOT IN ( \
           SELECT id FROM file_snapshots \
           WHERE file_id = ?1 AND label = 'auto' \
           ORDER BY revn DESC LIMIT ?2 \
         )",
        params![file_id.as_bytes(), keep as i64],
    )?;
    Ok(())
}

fn label_for_changes(changes: &[Value]) -> Option<String> {
    if changes.is_empty() {
        return None;
    }
    // Distinct change types as a comma-separated label — useful in DB
    // diagnostics. Penpot doesn't surface this in the UI; it's purely
    // for debugging.
    let mut kinds: Vec<&str> = changes
        .iter()
        .filter_map(|c| c.get("type").and_then(Value::as_str))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    Some(kinds.join(","))
}

// ───────────────────────── Row → struct ─────────────────────────

fn row_uuid(row: &Row<'_>, idx: usize) -> rusqlite::Result<Uuid> {
    let bytes: Vec<u8> = row.get(idx)?;
    Uuid::from_slice(&bytes).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Blob, Box::new(e))
    })
}

fn row_to_team(row: &Row<'_>) -> rusqlite::Result<Team> {
    let features: String = row.get(3)?;
    Ok(Team {
        id: row_uuid(row, 0)?,
        name: row.get(1)?,
        is_default: row.get::<_, i64>(2)? != 0,
        features: serde_json::from_str(&features).unwrap_or_default(),
        created_at: ts_to_dt(row.get::<_, i64>(4)?),
        modified_at: ts_to_dt(row.get::<_, i64>(5)?),
    })
}

fn row_to_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row_uuid(row, 0)?,
        team_id: row_uuid(row, 1)?,
        name: row.get(2)?,
        is_default: row.get::<_, i64>(3)? != 0,
        is_pinned: row.get::<_, i64>(4)? != 0,
        created_at: ts_to_dt(row.get::<_, i64>(5)?),
        modified_at: ts_to_dt(row.get::<_, i64>(6)?),
    })
}

fn row_to_file(row: &Row<'_>) -> rusqlite::Result<File> {
    let data_blob: Vec<u8> = row.get(8)?;
    let data_format: String = row.get(9)?;
    let data = db::decode_data(&data_blob, &data_format).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Blob, Box::new(io_err(e)))
    })?;
    let features_str: String = row.get(7)?;
    Ok(File {
        id: row_uuid(row, 0)?,
        project_id: row_uuid(row, 1)?,
        name: row.get(2)?,
        revn: row.get(3)?,
        vern: row.get(4)?,
        version: row.get::<_, i64>(5)? as u32,
        is_shared: row.get::<_, i64>(6)? != 0,
        features: serde_json::from_str(&features_str).unwrap_or_default(),
        created_at: ts_to_dt(row.get::<_, i64>(10)?),
        modified_at: ts_to_dt(row.get::<_, i64>(11)?),
        data,
    })
}

fn row_to_change_record(row: &Row<'_>) -> rusqlite::Result<ChangeRecord> {
    let changes_blob: Vec<u8> = row.get(2)?;
    let undo_blob: Vec<u8> = row.get(3).unwrap_or_default();
    Ok(ChangeRecord {
        revn: row.get(0)?,
        session_id: row_uuid(row, 1)?,
        changes: serde_json::from_slice(&changes_blob).unwrap_or(Value::Null),
        undo: serde_json::from_slice(&undo_blob).unwrap_or(Value::Null),
        at: ts_to_dt(row.get::<_, i64>(4)?),
    })
}

fn ts_to_dt(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_else(Utc::now)
}

fn io_err(e: anyhow::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::model;
    use serde_json::json;

    fn fresh_file(project_id: Uuid) -> File {
        model::File::empty(Uuid::new_v4(), project_id, "test")
    }

    #[test]
    fn seeded_has_default_team_and_project() {
        let s = Store::in_memory();
        let teams = s.list_teams();
        assert_eq!(teams.len(), 1);
        let projects = s.list_projects(teams[0].id);
        assert_eq!(projects.len(), 1);
    }

    #[test]
    fn put_and_fetch_file_inmemory() {
        let s = Store::in_memory();
        let f = fresh_file(LOCAL_PROJECT_ID);
        let id = f.id;
        s.put_file(f);
        assert!(s.get_file(id).is_some());
    }

    #[test]
    fn sqlite_seeds_default_team_and_project() {
        let s = Store::in_memory_sqlite().unwrap();
        let teams = s.list_teams();
        assert_eq!(teams.len(), 1);
        assert_eq!(teams[0].id, LOCAL_TEAM_ID);
        let projects = s.list_projects(teams[0].id);
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, LOCAL_PROJECT_ID);
    }

    #[test]
    fn sqlite_round_trips_file() {
        let s = Store::in_memory_sqlite().unwrap();
        let f = fresh_file(LOCAL_PROJECT_ID);
        let id = f.id;
        let original_name = f.name.clone();
        s.put_file(f);
        let back = s.get_file(id).expect("file persisted");
        assert_eq!(back.id, id);
        assert_eq!(back.name, original_name);
        assert_eq!(back.revn, 0);
        // pages_index round-trips through json+zstd
        let pages_index = back
            .data
            .get("pagesIndex")
            .and_then(Value::as_object)
            .expect("pagesIndex");
        assert_eq!(pages_index.len(), 1);
    }

    #[test]
    fn sqlite_apply_update_bumps_revn() {
        let s = Store::in_memory_sqlite().unwrap();
        let f = fresh_file(LOCAL_PROJECT_ID);
        let id = f.id;
        let page_id = first_page_id(&f);
        s.put_file(f);

        let new_obj_id = Uuid::new_v4().to_string();
        let outcome = s.apply_update(UpdateRequest {
            file_id: id,
            client_revn: 0,
            session_id: Uuid::new_v4(),
            changes: vec![json!({
                "type": "add-obj",
                "id": new_obj_id,
                "pageId": page_id,
                "parentId": Uuid::nil().to_string(),
                "frameId": Uuid::nil().to_string(),
                "obj": {"id": new_obj_id, "type": "rect", "x": 1.0}
            })],
        });
        match outcome {
            UpdateOutcome::Applied { new_revn, .. } => assert_eq!(new_revn, 1),
            other => panic!("expected applied, got {other:?}"),
        }
        let updated = s.get_file(id).unwrap();
        assert_eq!(updated.revn, 1);
        let objects = updated
            .data
            .pointer(&format!("/pagesIndex/{page_id}/objects"))
            .and_then(Value::as_object)
            .unwrap();
        assert!(objects.contains_key(&new_obj_id));
    }

    #[test]
    fn sqlite_apply_update_lagged_on_revn_mismatch() {
        let s = Store::in_memory_sqlite().unwrap();
        let f = fresh_file(LOCAL_PROJECT_ID);
        let id = f.id;
        s.put_file(f);
        // First apply: bumps to revn=1
        let page_id = first_page_id(&s.get_file(id).unwrap());
        let id_a = Uuid::new_v4().to_string();
        let _ = s.apply_update(UpdateRequest {
            file_id: id,
            client_revn: 0,
            session_id: Uuid::new_v4(),
            changes: vec![json!({
                "type": "add-obj",
                "id": id_a,
                "pageId": page_id,
                "parentId": Uuid::nil().to_string(),
                "obj": {"id": id_a, "type": "rect"}
            })],
        });
        // Second apply with stale revn=0 → lagged, missed contains the first change.
        let outcome = s.apply_update(UpdateRequest {
            file_id: id,
            client_revn: 0,
            session_id: Uuid::new_v4(),
            changes: vec![],
        });
        match outcome {
            UpdateOutcome::Lagged { server_revn, missed } => {
                assert_eq!(server_revn, 1);
                assert_eq!(missed.len(), 1);
                assert_eq!(missed[0].get("id").and_then(Value::as_str), Some(id_a.as_str()));
            }
            other => panic!("expected lagged, got {other:?}"),
        }
    }

    #[test]
    fn sqlite_auto_snapshot_every_n_revns() {
        let s = Store::in_memory_sqlite().unwrap();
        let f = fresh_file(LOCAL_PROJECT_ID);
        let id = f.id;
        let page_id = first_page_id(&f);
        s.put_file(f);

        for i in 0..AUTO_SNAPSHOT_EVERY {
            let obj_id = Uuid::new_v4().to_string();
            let outcome = s.apply_update(UpdateRequest {
                file_id: id,
                client_revn: i,
                session_id: Uuid::new_v4(),
                changes: vec![json!({
                    "type": "add-obj",
                    "id": obj_id,
                    "pageId": page_id,
                    "parentId": Uuid::nil().to_string(),
                    "obj": {"id": obj_id, "type": "rect"}
                })],
            });
            assert!(matches!(outcome, UpdateOutcome::Applied { .. }));
        }
        // Auto-snapshot should exist now
        let backend = match &*s.backend {
            Backend::Sqlite(c) => c,
            _ => panic!("not sqlite"),
        };
        let conn = backend.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_snapshots WHERE file_id = ?1 AND label = 'auto'",
                params![id.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn sqlite_change_log_is_persisted() {
        let s = Store::in_memory_sqlite().unwrap();
        let f = fresh_file(LOCAL_PROJECT_ID);
        let id = f.id;
        let page_id = first_page_id(&f);
        s.put_file(f);
        let obj_id = Uuid::new_v4().to_string();
        let _ = s.apply_update(UpdateRequest {
            file_id: id,
            client_revn: 0,
            session_id: Uuid::new_v4(),
            changes: vec![json!({
                "type": "add-obj",
                "id": obj_id,
                "pageId": page_id,
                "parentId": Uuid::nil().to_string(),
                "obj": {"id": obj_id, "type": "rect"}
            })],
        });
        let log = s.change_log(id);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].revn, 1);
    }

    #[test]
    fn sqlite_delete_is_soft_and_reappears_after_put() {
        let s = Store::in_memory_sqlite().unwrap();
        let f = fresh_file(LOCAL_PROJECT_ID);
        let id = f.id;
        s.put_file(f.clone());
        assert!(s.get_file(id).is_some());
        s.delete_file(id);
        assert!(s.get_file(id).is_none());
        // Re-putting clears deleted_at.
        s.put_file(f);
        assert!(s.get_file(id).is_some());
    }

    fn first_page_id(file: &File) -> String {
        file.data
            .get("pagesIndex")
            .and_then(Value::as_object)
            .and_then(|m| m.keys().next().cloned())
            .unwrap()
    }

    #[test]
    fn sqlite_store_and_read_media() {
        let s = Store::in_memory_sqlite().unwrap();
        let stored = s
            .store_media(b"hello-image", "image/png", None)
            .expect("store");
        let bytes = s.read_media(stored.id).expect("read");
        assert_eq!(bytes, b"hello-image");
        let meta = s.media_metadata(stored.id).expect("metadata");
        assert_eq!(meta.size, 11);
    }

    #[test]
    fn memory_store_and_read_media() {
        let s = Store::in_memory();
        let stored = s
            .store_media(b"in-memory-bytes", "image/jpeg", None)
            .expect("store");
        let bytes = s.read_media(stored.id).expect("read");
        assert_eq!(bytes, b"in-memory-bytes");
    }

    #[test]
    fn media_provider_serves_stored_bytes() {
        let s = Store::in_memory_sqlite().unwrap();
        let id = Uuid::new_v4();
        s.store_media(b"X", "image/png", Some(id)).unwrap();
        let provider = s.media_provider();
        assert_eq!(provider(&id.to_string()).as_deref(), Some(b"X".as_ref()));
        assert!(provider("not-a-uuid").is_none());
        assert!(provider(&Uuid::new_v4().to_string()).is_none());
    }
}
