CREATE TABLE IF NOT EXISTS directories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE COLLATE NOCASE
);

CREATE TABLE IF NOT EXISTS albums (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    artist TEXT,
    year INTEGER,
    UNIQUE(title, artist)
);

CREATE TABLE IF NOT EXISTS tracks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE COLLATE NOCASE,
    title TEXT,
    artist TEXT,
    album_id INTEGER REFERENCES albums(id) ON DELETE SET NULL,
    directory_id INTEGER REFERENCES directories(id) ON DELETE CASCADE,
    track_number INTEGER,
    duration_secs REAL,
    modified_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tracks_directory ON tracks(directory_id);
CREATE INDEX IF NOT EXISTS idx_tracks_album ON tracks(album_id);
