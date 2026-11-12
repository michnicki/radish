-- Force one full re-read of every file after an upgrade. Values backfilled from
-- older schemas (such as a year copied off a shared album row) are not the
-- file's own tags, and the incremental import skips any file whose stored
-- modification time still matches. No file has a modification time of -1, so
-- the next import reads each file once and replaces those values with the real
-- tags.
UPDATE tracks SET modified_at = -1;
