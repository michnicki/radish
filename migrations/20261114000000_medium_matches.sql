-- A local release that holds one disc of a multi-disc release (a folder per
-- disc) is matched to that disc. NULL means the files match the whole release,
-- as every earlier match does.
ALTER TABLE release_matches ADD COLUMN medium_position INTEGER
    CHECK (medium_position IS NULL OR medium_position > 0);
