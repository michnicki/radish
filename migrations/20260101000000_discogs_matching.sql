-- Discogs matching: local release identity, observed metadata, accepted overlay,
-- and the review queue.
--
-- The pre-migration schema is created by `src/schema.sql` before this migration
-- runs, so legacy databases only receive these additive changes.

CREATE TABLE local_releases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    directory_path TEXT NOT NULL COLLATE NOCASE,
    album_title_norm TEXT NOT NULL,
    album_title TEXT,
    album_artist TEXT,
    artist_kind TEXT NOT NULL DEFAULT 'unknown',
    year INTEGER,
    barcode TEXT,
    catalog_number TEXT,
    embedded_discogs_id INTEGER,
    is_compilation INTEGER NOT NULL DEFAULT 0,
    has_album_tag INTEGER NOT NULL DEFAULT 1,
    track_count INTEGER NOT NULL DEFAULT 0,
    source_revision TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'unmatched',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(directory_path, album_title_norm),
    CHECK (status IN ('unmatched', 'matched', 'stale'))
);

ALTER TABLE tracks ADD COLUMN local_release_id INTEGER REFERENCES local_releases(id) ON DELETE SET NULL;
ALTER TABLE tracks ADD COLUMN album_artist TEXT;
ALTER TABLE tracks ADD COLUMN disc_number INTEGER;
ALTER TABLE tracks ADD COLUMN barcode TEXT;
ALTER TABLE tracks ADD COLUMN catalog_number TEXT;
ALTER TABLE tracks ADD COLUMN discogs_release_id INTEGER;

CREATE INDEX idx_local_releases_directory ON local_releases(directory_path);
CREATE INDEX idx_tracks_local_release ON tracks(local_release_id);

CREATE TABLE discogs_matches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    local_release_id INTEGER NOT NULL UNIQUE REFERENCES local_releases(id) ON DELETE CASCADE,
    discogs_release_id INTEGER NOT NULL,
    discogs_master_id INTEGER,
    release_url TEXT NOT NULL,
    title TEXT,
    artist TEXT,
    year INTEGER,
    country TEXT,
    label TEXT,
    catalog_number TEXT,
    format TEXT,
    barcode TEXT,
    score REAL NOT NULL,
    algorithm_version TEXT NOT NULL,
    acceptance_method TEXT NOT NULL,
    accepted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    source_revision TEXT NOT NULL,
    stale INTEGER NOT NULL DEFAULT 0,
    metadata_json TEXT NOT NULL,
    CHECK (acceptance_method IN ('automatic', 'manual'))
);

CREATE TABLE discogs_track_matches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    local_release_id INTEGER NOT NULL REFERENCES local_releases(id) ON DELETE CASCADE,
    track_id INTEGER NOT NULL UNIQUE REFERENCES tracks(id) ON DELETE CASCADE,
    remote_ordinal INTEGER NOT NULL,
    remote_position TEXT,
    remote_title TEXT,
    remote_artist TEXT,
    remote_duration_secs REAL,
    UNIQUE (local_release_id, remote_ordinal)
);

CREATE INDEX idx_discogs_track_matches_release ON discogs_track_matches(local_release_id);

CREATE TABLE match_reviews (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    local_release_id INTEGER NOT NULL UNIQUE REFERENCES local_releases(id) ON DELETE CASCADE,
    candidates_json TEXT NOT NULL,
    proposed_mapping_json TEXT NOT NULL,
    reasons_json TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
