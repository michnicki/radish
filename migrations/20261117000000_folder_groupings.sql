-- Folder merging. A user's grouping decision per unit (a folder, or a folder
-- with two or more disc subfolders), and the unit each local release belongs
-- to. A unit without a decision is grouped by the automatic rule.
CREATE TABLE folder_groupings (
    directory_path TEXT PRIMARY KEY COLLATE NOCASE,
    mode TEXT NOT NULL CHECK (mode IN ('merge', 'separate')),
    decided_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

-- Every existing release is in its own folder's unit until the next regroup
-- says otherwise; only releases in disc subfolders change then.
ALTER TABLE local_releases ADD COLUMN unit_path TEXT NOT NULL DEFAULT '' COLLATE NOCASE;
UPDATE local_releases SET unit_path = directory_path;
CREATE INDEX idx_local_releases_unit ON local_releases(unit_path);
