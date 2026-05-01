-- V3: redesign font storage.
--
-- V2's `fonts` table modelled one media row per variant, but Penpot
-- variants ship multiple format files (TTF, OTF, WOFF, WOFF2) and the
-- frontend negotiates by browser support. We redo the table to mirror
-- Penpot's `font_variant` shape: one row per variant, four optional
-- `*_file_id` foreign keys into `media`.

DROP TABLE IF EXISTS fonts;

CREATE TABLE font_variants (
    id             BLOB PRIMARY KEY,
    team_id        BLOB NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    -- Family group id. Penpot uses one `font_id` for all variants of
    -- the same family (regular / italic / bold / …) so the frontend
    -- can collapse them in the picker.
    font_id        BLOB NOT NULL,
    font_family    TEXT NOT NULL,
    font_weight    INTEGER NOT NULL,
    -- 'normal' | 'italic' (we treat any other value as 'normal')
    font_style     TEXT NOT NULL DEFAULT 'normal',
    woff1_file_id  BLOB REFERENCES media(id),
    woff2_file_id  BLOB REFERENCES media(id),
    ttf_file_id    BLOB REFERENCES media(id),
    otf_file_id    BLOB REFERENCES media(id),
    created_at     INTEGER NOT NULL,
    deleted_at     INTEGER
) STRICT;
CREATE INDEX idx_font_variants_team ON font_variants(team_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_font_variants_font ON font_variants(font_id) WHERE deleted_at IS NULL;
