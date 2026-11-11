-- Year is a per-file tag, not a property of an album row shared by every folder
-- with the same title and artist. Move it onto the tracks, then give albums a
-- real identity: NULL and empty artists are the same artist.
ALTER TABLE tracks ADD COLUMN year INTEGER;

-- Backfill before repointing, while every track still references its own row.
UPDATE tracks
SET year = (SELECT a.year FROM albums a WHERE a.id = tracks.album_id)
WHERE album_id IS NOT NULL;

UPDATE tracks
SET album_id = (
    SELECT MIN(keep.id)
    FROM albums cur
    JOIN albums keep
        ON keep.title = cur.title AND IFNULL(keep.artist, '') = IFNULL(cur.artist, '')
    WHERE cur.id = tracks.album_id
)
WHERE album_id IS NOT NULL;

DELETE FROM albums
WHERE id NOT IN (SELECT MIN(id) FROM albums GROUP BY title, IFNULL(artist, ''));

-- Retired: tracks.year is authoritative.
UPDATE albums SET year = NULL;

CREATE UNIQUE INDEX idx_albums_identity ON albums(title, IFNULL(artist, ''));
