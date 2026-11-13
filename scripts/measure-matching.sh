#!/usr/bin/env bash
# Rerun `radish match --offline` from scratch on a copy of a library database.
#
#   scripts/measure-matching.sh LABEL SOURCE_DB OUT_DIR
#
# Builds the current working tree in release mode, copies SOURCE_DB (never
# touching it), deletes every accepted match and queued review from the copy,
# resets every release to unmatched, then matches offline with no network and
# no credentials. Writes OUT_DIR/LABEL.{db,log,time,accepted.tsv}.
set -euo pipefail

label=${1:?label}
source_db=${2:?source database}
out=${3:?output directory}

repo=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out/bin" "$out/work-$label"

CARGO_TARGET_DIR="$out/target" cargo build --release --locked --manifest-path "$repo/Cargo.toml"
cp "$out/target/release/radish" "$out/bin/radish-$label"

db="$out/$label.db"
rm -f "$db" "$db-shm" "$db-wal"
sqlite3 "$source_db" ".backup '$db'"
sqlite3 "$db" "PRAGMA foreign_keys=ON; DELETE FROM match_reviews; DELETE FROM release_matches; UPDATE local_releases SET status='unmatched';"

# No .env in this directory, an empty environment apart from the index location:
# the run cannot reach a provider or read a credential.
start=$(date +%s)
(
  cd "$out/work-$label"
  env -i HOME="$HOME" PATH="$PATH" RADISH_INDEX_DIR="$repo/radish-index" \
    "$out/bin/radish-$label" --db "$db" match --offline
) >"$out/$label.log" 2>&1
echo "$label wall_seconds=$(( $(date +%s) - start ))" | tee "$out/$label.time"

# A disc matched on its own records its medium; older schemas lack the column.
medium_column=""
if sqlite3 "$db" "PRAGMA table_info(release_matches)" | grep -q '|medium_position|'; then
  medium_column=", medium_position"
fi
sqlite3 -tabs "$db" \
  "SELECT local_release_id, provider, remote_release_id, match_scope, score, algorithm_version$medium_column
     FROM release_matches ORDER BY local_release_id" >"$out/$label.accepted.tsv"
echo "release status counts:"
sqlite3 "$db" "SELECT status, COUNT(*) FROM local_releases GROUP BY status"
echo "reviews queued: $(sqlite3 "$db" 'SELECT COUNT(DISTINCT local_release_id) FROM match_reviews')"
echo "accepted: $(wc -l <"$out/$label.accepted.tsv")"
