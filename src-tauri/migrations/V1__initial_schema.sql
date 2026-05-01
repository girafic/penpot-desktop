-- Initial schema for the offline Penpot store. Mirrors Penpots
-- PostgreSQL backend in spirit; UUIDs are stored as 16-byte BLOBs and
-- timestamps as INTEGER unix-millis (stable across SQLite versions and
-- timezones, and trivial to convert via chrono::DateTime::from_timestamp_millis).
--
-- The big file-data tree lives as a zstd-compressed JSON blob in
-- `files.data`. This mirrors Penpots `file.data` BYTEA column. We could
-- shard pages into their own table later (see notes in backend/db.rs)
-- but for files <50 MB the single-blob approach is faster and simpler.

CREATE TABLE teams (
    id          BLOB PRIMARY KEY,
    name        TEXT NOT NULL,
    is_default  INTEGER NOT NULL DEFAULT 0,
    features    TEXT NOT NULL DEFAULT '[]',
    created_at  INTEGER NOT NULL,
    modified_at INTEGER NOT NULL
) STRICT;

CREATE TABLE projects (
    id          BLOB PRIMARY KEY,
    team_id     BLOB NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    is_pinned   INTEGER NOT NULL DEFAULT 0,
    is_default  INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    modified_at INTEGER NOT NULL,
    deleted_at  INTEGER
) STRICT;
CREATE INDEX idx_projects_team ON projects(team_id) WHERE deleted_at IS NULL;

CREATE TABLE files (
    id           BLOB PRIMARY KEY,
    project_id   BLOB NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    revn         INTEGER NOT NULL DEFAULT 0,
    vern         INTEGER NOT NULL DEFAULT 0,
    version      INTEGER NOT NULL,
    is_shared    INTEGER NOT NULL DEFAULT 0,
    features     TEXT NOT NULL DEFAULT '[]',
    -- zstd(serde_json::to_vec(file.data)) — see backend/db.rs for codec.
    data         BLOB NOT NULL,
    data_format  TEXT NOT NULL DEFAULT 'json+zstd',
    created_at   INTEGER NOT NULL,
    modified_at  INTEGER NOT NULL,
    deleted_at   INTEGER
) STRICT;
CREATE INDEX idx_files_project ON files(project_id) WHERE deleted_at IS NULL;

-- Per-file change log. Each row is one applied `update-file` payload.
-- Used for redo/undo and "lagged?" replay if multiple browser tabs ever
-- edit the same file. WITHOUT ROWID + composite PK because revn is
-- always queried alongside file_id.
CREATE TABLE file_changes (
    file_id      BLOB NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    revn         INTEGER NOT NULL,
    session_id   BLOB NOT NULL,
    -- Origin marker; useful when persistent multi-tab editing arrives.
    commit_id    BLOB NOT NULL,
    label        TEXT,
    -- JSON-encoded change vector.
    changes      BLOB NOT NULL,
    undo_changes BLOB,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (file_id, revn)
) STRICT, WITHOUT ROWID;

-- Snapshots: full-data captures taken automatically every N revns and
-- on user-triggered "Pin Version". Manual snapshots have a non-null
-- label and never get pruned; auto snapshots have label='auto' and we
-- keep at most AUTO_SNAPSHOT_KEEP of them.
CREATE TABLE file_snapshots (
    id          BLOB PRIMARY KEY,
    file_id     BLOB NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    revn        INTEGER NOT NULL,
    label       TEXT,
    data        BLOB NOT NULL,
    data_format TEXT NOT NULL DEFAULT 'json+zstd',
    created_at  INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_snapshots_file_revn ON file_snapshots(file_id, revn DESC);
CREATE INDEX idx_snapshots_label ON file_snapshots(file_id, label);

-- Cross-file shared library references. Empty in the single-user mode but
-- the schema is in place so Phase 3 can wire shared libraries without
-- another migration.
CREATE TABLE file_library_rels (
    file_id    BLOB NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    library_id BLOB NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    synced_at  INTEGER,
    PRIMARY KEY (file_id, library_id)
) STRICT, WITHOUT ROWID;

-- Content-addressed media metadata. Bytes live on disk under
-- <app_data>/media/<sha[0..2]>/<sha>; this table stores dimensions and
-- refcounts for cleanup. Phase 3 wires the upload endpoint.
CREATE TABLE media (
    sha256     TEXT PRIMARY KEY,
    size       INTEGER NOT NULL,
    mime_type  TEXT NOT NULL,
    width      INTEGER,
    height     INTEGER,
    refcount   INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE file_media_refs (
    file_id      BLOB NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    media_sha256 TEXT NOT NULL REFERENCES media(sha256),
    -- :id of the corresponding asset entry inside file.data.media.
    asset_id     BLOB NOT NULL,
    name         TEXT NOT NULL,
    PRIMARY KEY (file_id, asset_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE fonts (
    id           BLOB PRIMARY KEY,
    team_id      BLOB NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    family       TEXT NOT NULL,
    style        TEXT NOT NULL,
    weight       INTEGER NOT NULL,
    media_sha256 TEXT NOT NULL REFERENCES media(sha256),
    created_at   INTEGER NOT NULL
) STRICT;

-- Generic key/value store for backend metadata that doesn't deserve its
-- own table (last-modified-by, install-id, schema-feature flags …).
CREATE TABLE kv (
    k TEXT PRIMARY KEY,
    v BLOB NOT NULL
) STRICT, WITHOUT ROWID;
