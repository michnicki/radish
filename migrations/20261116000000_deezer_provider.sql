-- no-transaction
-- Deezer becomes a third provider: 'deezer' joins the provider CHECK of the
-- accepted matches and the review queue. SQLite cannot change a CHECK in
-- place, so both tables are rebuilt with SQLite's documented procedure.
--
-- Foreign keys are switched off first, outside any transaction (the pragma is
-- a no-op inside one, hence `-- no-transaction` above). With them on,
-- dropping release_matches would cascade-delete every release_track_matches
-- row and set every match_reviews.expected_match_id to NULL. Every column,
-- default and CHECK is copied from the current tables, including the ALTER-added
-- match_scope and medium_position, and every row keeps its id.
PRAGMA foreign_keys = OFF;

BEGIN;

CREATE TABLE release_matches_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    local_release_id INTEGER NOT NULL UNIQUE REFERENCES local_releases(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    remote_release_id TEXT NOT NULL,
    remote_group_id TEXT,
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
    snapshot_version INTEGER NOT NULL,
    metadata_json TEXT NOT NULL,
    match_scope TEXT NOT NULL DEFAULT 'edition'
        CHECK (match_scope IN ('album', 'edition')),
    medium_position INTEGER
        CHECK (medium_position IS NULL OR medium_position > 0),
    CHECK (provider IN ('discogs', 'musicbrainz', 'deezer')),
    CHECK (acceptance_method IN ('automatic', 'manual'))
);

INSERT INTO release_matches_new (
    id, local_release_id, provider, remote_release_id, remote_group_id, release_url,
    title, artist, year, country, label, catalog_number, format, barcode, score,
    algorithm_version, acceptance_method, accepted_at, source_revision, stale,
    snapshot_version, metadata_json, match_scope, medium_position
)
SELECT
    id, local_release_id, provider, remote_release_id, remote_group_id, release_url,
    title, artist, year, country, label, catalog_number, format, barcode, score,
    algorithm_version, acceptance_method, accepted_at, source_revision, stale,
    snapshot_version, metadata_json, match_scope, medium_position
FROM release_matches;

CREATE TABLE match_reviews_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    local_release_id INTEGER NOT NULL REFERENCES local_releases(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    snapshot_version INTEGER NOT NULL,
    expected_match_id INTEGER REFERENCES release_matches(id) ON DELETE SET NULL,
    candidates_json TEXT NOT NULL,
    proposed_mapping_json TEXT NOT NULL,
    reasons_json TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE (local_release_id, provider),
    CHECK (provider IN ('discogs', 'musicbrainz', 'deezer'))
);

INSERT INTO match_reviews_new (
    id, local_release_id, provider, snapshot_version, expected_match_id,
    candidates_json, proposed_mapping_json, reasons_json, source_revision,
    created_at, updated_at
)
SELECT
    id, local_release_id, provider, snapshot_version, expected_match_id,
    candidates_json, proposed_mapping_json, reasons_json, source_revision,
    created_at, updated_at
FROM match_reviews;

-- Dropping a table deletes its sqlite_sequence row, which would let the ids of
-- deleted matches and reviews be handed out again. Both new tables are filled
-- now (each copy moved their counters), so give them the old tables' counters,
-- which RENAME then carries over.
DELETE FROM sqlite_sequence WHERE name IN ('release_matches_new', 'match_reviews_new');
INSERT INTO sqlite_sequence (name, seq)
SELECT name || '_new', seq FROM sqlite_sequence
WHERE name IN ('release_matches', 'match_reviews');

DROP TABLE match_reviews;
DROP TABLE release_matches;
ALTER TABLE release_matches_new RENAME TO release_matches;
ALTER TABLE match_reviews_new RENAME TO match_reviews;

CREATE INDEX idx_release_matches_provider ON release_matches(provider);
CREATE INDEX idx_release_matches_stale ON release_matches(stale);
CREATE INDEX idx_match_reviews_expected_match ON match_reviews(expected_match_id);

COMMIT;

PRAGMA foreign_keys = ON;

-- The copy leaves a WAL as large as the tables it copied. Radish never closes
-- its pool, so SQLite would not delete it; truncate it now. A reader that
-- blocks the checkpoint only makes it report busy, which does not fail the
-- migration.
PRAGMA wal_checkpoint(TRUNCATE);
