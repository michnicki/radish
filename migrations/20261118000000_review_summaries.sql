-- A summary of each saved review's first candidate, so the review queue can
-- sort and filter without reading the candidates' JSON. The columns sit
-- after `candidates_json` in each row, so queries read them through
-- `idx_match_reviews_summary`; `top_can_apply` is NULL until a row is
-- summarized (see `store::fill_review_summaries`).
ALTER TABLE match_reviews ADD COLUMN top_score REAL;
ALTER TABLE match_reviews ADD COLUMN top_can_apply INTEGER;
ALTER TABLE match_reviews ADD COLUMN top_reason TEXT;
ALTER TABLE match_reviews ADD COLUMN top_blockers INTEGER;
CREATE INDEX idx_match_reviews_summary ON match_reviews(
    local_release_id, provider, top_can_apply, top_score, source_revision);
