-- MusicBrainz matching: one provider-qualified accepted overlay, a generic
-- mapping table, provider-keyed reviews, and observed MusicBrainz identifiers.
--
-- Existing Discogs rows are copied before their tables are dropped. Historical
-- snapshots keep `snapshot_version = 1` so they decode through the legacy
-- Discogs codec; new writes are version 2.

CREATE TABLE release_matches (
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
    CHECK (provider IN ('discogs', 'musicbrainz')),
    CHECK (acceptance_method IN ('automatic', 'manual'))
);

CREATE INDEX idx_release_matches_provider ON release_matches(provider);
CREATE INDEX idx_release_matches_stale ON release_matches(stale);

CREATE TABLE release_track_matches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    match_id INTEGER NOT NULL REFERENCES release_matches(id) ON DELETE CASCADE,
    track_id INTEGER NOT NULL UNIQUE REFERENCES tracks(id) ON DELETE CASCADE,
    remote_ordinal INTEGER NOT NULL,
    remote_position TEXT,
    remote_title TEXT,
    remote_artist TEXT,
    remote_duration_secs REAL,
    medium_position INTEGER,
    track_position INTEGER,
    remote_track_id TEXT,
    remote_recording_id TEXT,
    UNIQUE (match_id, remote_ordinal)
);

CREATE INDEX idx_release_track_matches_match ON release_track_matches(match_id);

-- Copy accepted Discogs overlays before dropping the old tables. Numeric
-- service IDs become TEXT provider-local identifiers; timestamps, scores,
-- revisions, staleness, and snapshots are preserved.
INSERT INTO release_matches (
    id, local_release_id, provider, remote_release_id, remote_group_id, release_url,
    title, artist, year, country, label, catalog_number, format, barcode, score,
    algorithm_version, acceptance_method, accepted_at, source_revision, stale,
    snapshot_version, metadata_json
)
SELECT
    id, local_release_id, 'discogs', CAST(discogs_release_id AS TEXT),
    CASE WHEN discogs_master_id IS NULL THEN NULL ELSE CAST(discogs_master_id AS TEXT) END,
    release_url, title, artist, year, country, label, catalog_number, format, barcode,
    score, algorithm_version, acceptance_method, accepted_at, source_revision, stale,
    1, metadata_json
FROM discogs_matches;

INSERT INTO release_track_matches (
    match_id, track_id, remote_ordinal, remote_position, remote_title, remote_artist,
    remote_duration_secs, medium_position, track_position, remote_track_id, remote_recording_id
)
SELECT
    rm.id, dtm.track_id, dtm.remote_ordinal, dtm.remote_position, dtm.remote_title,
    dtm.remote_artist, dtm.remote_duration_secs, NULL, NULL, NULL, NULL
FROM discogs_track_matches dtm
JOIN discogs_matches dm ON dm.local_release_id = dtm.local_release_id
JOIN release_matches rm
    ON rm.local_release_id = dm.local_release_id AND rm.provider = 'discogs';

-- Rebuild the review queue with a provider key and snapshot version. Existing
-- rows are Discogs/version 1, and point at their accepted match when one exists.
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
    CHECK (provider IN ('discogs', 'musicbrainz'))
);

INSERT INTO match_reviews_new (
    id, local_release_id, provider, snapshot_version, expected_match_id,
    candidates_json, proposed_mapping_json, reasons_json, source_revision,
    created_at, updated_at
)
SELECT
    mr.id, mr.local_release_id, 'discogs', 1,
    (SELECT rm.id FROM release_matches rm WHERE rm.local_release_id = mr.local_release_id),
    mr.candidates_json, mr.proposed_mapping_json, mr.reasons_json, mr.source_revision,
    mr.created_at, mr.updated_at
FROM match_reviews mr;

DROP TABLE match_reviews;
ALTER TABLE match_reviews_new RENAME TO match_reviews;

DROP TABLE discogs_track_matches;
DROP TABLE discogs_matches;

-- Observed values stay separate from accepted overlay values. Invalid and
-- conflicting observations live in the JSON arrays and diagnostics columns.
ALTER TABLE tracks ADD COLUMN musicbrainz_ids_json TEXT NOT NULL
    DEFAULT '{"release":[],"release_group":[],"release_track":[],"recording":[]}';
ALTER TABLE local_releases ADD COLUMN embedded_musicbrainz_release_id TEXT;
ALTER TABLE local_releases ADD COLUMN embedded_musicbrainz_release_group_id TEXT;
ALTER TABLE local_releases ADD COLUMN musicbrainz_problems_json TEXT NOT NULL DEFAULT '[]';
