-- V2: media storage redesign.
--
-- V1 had `media(sha256 PK)` with the intent to dedup by content hash. In
-- practice Penpot's file-data references media by `:id` (a UUID storage
-- object id), not by SHA, and the binfile-v3 archive layout writes
-- `media/<storage-id>` entries — so we need a UUID primary key on the
-- on-disk file mapping. Content-addressed dedup stays as a side index.
--
-- Safe to drop/recreate: V1 never wrote rows here (Phase 3 wires the
-- upload endpoint for the first time). If anyone shipped V1 with media
-- data this migration would destroy it; we accept that since the offline
-- backend hasn't reached release status.

-- Drop in dependency order: fonts referenced media via sha256 in V1.
DROP TABLE IF EXISTS fonts;
DROP TABLE IF EXISTS file_media_refs;
DROP TABLE IF EXISTS media;

-- One row per uploaded asset. `id` is what file.data.media[*].id points
-- at. Bytes live on disk under <app_data>/media/<id[0..2]>/<id> so the
-- directory tree never balloons past a few hundred entries per shard.
CREATE TABLE media (
    id          BLOB PRIMARY KEY,
    sha256      TEXT NOT NULL,
    size        INTEGER NOT NULL,
    mime_type   TEXT NOT NULL,
    width       INTEGER,
    height      INTEGER,
    created_at  INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_media_sha ON media(sha256);

-- Per-file references: which assets a file's data tree points at.
-- Keeps the option open of cross-file dedup at the storage layer
-- without forcing it (file-A and file-B sharing one media row).
CREATE TABLE file_media_refs (
    file_id    BLOB NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    media_id   BLOB NOT NULL REFERENCES media(id),
    asset_id   BLOB NOT NULL,
    name       TEXT NOT NULL,
    PRIMARY KEY (file_id, asset_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX idx_file_media_refs_media ON file_media_refs(media_id);

-- Re-create fonts referencing the new media UUID PK. Empty in
-- single-user mode but kept around for the upcoming font-upload
-- endpoint (Penpot's `create-font-variant`).
CREATE TABLE fonts (
    id           BLOB PRIMARY KEY,
    team_id      BLOB NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    family       TEXT NOT NULL,
    style        TEXT NOT NULL,
    weight       INTEGER NOT NULL,
    media_id     BLOB NOT NULL REFERENCES media(id),
    created_at   INTEGER NOT NULL
) STRICT;
