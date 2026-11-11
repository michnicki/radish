-- Preserve existing edition decisions and distinguish new album-only matches.
ALTER TABLE release_matches ADD COLUMN match_scope TEXT NOT NULL DEFAULT 'edition'
    CHECK (match_scope IN ('album', 'edition'));
