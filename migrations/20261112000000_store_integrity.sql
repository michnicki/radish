-- Overlays accepted before this version indexed a disc/track/path-ordered mapping
-- into tracks reloaded in path order. Mark matches whose path order differs stale.
WITH ranked AS (
    SELECT local_release_id,
           ROW_NUMBER() OVER (PARTITION BY local_release_id ORDER BY path) AS path_rank,
           ROW_NUMBER() OVER (
               PARTITION BY local_release_id
               ORDER BY COALESCE(disc_number, 1),
                        COALESCE(track_number, 9223372036854775807),
                        lower(path)
           ) AS track_rank
    FROM tracks
    WHERE local_release_id IS NOT NULL
)
UPDATE local_releases
SET status = 'stale', updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE status = 'matched'
  AND id IN (SELECT local_release_id FROM ranked WHERE path_rank <> track_rank)
  AND id IN (SELECT local_release_id FROM release_matches);

WITH ranked AS (
    SELECT local_release_id,
           ROW_NUMBER() OVER (PARTITION BY local_release_id ORDER BY path) AS path_rank,
           ROW_NUMBER() OVER (
               PARTITION BY local_release_id
               ORDER BY COALESCE(disc_number, 1),
                        COALESCE(track_number, 9223372036854775807),
                        lower(path)
           ) AS track_rank
    FROM tracks
    WHERE local_release_id IS NOT NULL
)
UPDATE release_matches
SET stale = 1
WHERE stale = 0
  AND local_release_id IN (SELECT local_release_id FROM ranked WHERE path_rank <> track_rank);

-- Drop overlay rows whose track no longer belongs to the match's release.
DELETE FROM release_track_matches
WHERE NOT EXISTS (
    SELECT 1
    FROM tracks t
    JOIN release_matches m ON m.local_release_id = t.local_release_id
    WHERE t.id = release_track_matches.track_id
      AND m.id = release_track_matches.match_id
);
