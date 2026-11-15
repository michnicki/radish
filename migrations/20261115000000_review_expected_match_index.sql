-- Deleting a release_matches row sets match_reviews.expected_match_id to NULL
-- wherever it names that match (ON DELETE SET NULL). Without this index SQLite
-- scans every review, candidate JSON included, once per deleted match: about
-- 0.15 s each on a library with 200,000 reviews.
CREATE INDEX IF NOT EXISTS idx_match_reviews_expected_match
    ON match_reviews(expected_match_id);
