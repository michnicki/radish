#!/usr/bin/env python3
"""Compare two `match --offline` runs made by measure-matching.sh.

usage: compare-matching.py BASE.accepted.tsv VARIANT.accepted.tsv VARIANT.db AUDIT.tsv

Prints lost, switched and new accepts. New accepts are split by provider and by
how the local album title relates to the remote one (exact, prefix, neither).
AUDIT.tsv lists every new accept whose titles are neither equal nor a prefix of
one another, plus a seeded random 30 of the rest, for reading by hand.
"""
import csv
import random
import re
import sqlite3
import sys
import unicodedata
from collections import Counter


def read(path):
    rows = {}
    with open(path, newline="") as handle:
        for row in csv.reader(handle, delimiter="\t"):
            rows[int(row[0])] = (row[1], row[2], row[3], float(row[4]), row[5])
    return rows


def norm(text):
    text = unicodedata.normalize("NFD", text or "")
    text = "".join(ch for ch in text if not unicodedata.combining(ch))
    return " ".join(re.sub(r"[^0-9a-z]+", " ", text.lower()).split())


def relation(local, remote):
    a, b = norm(local), norm(remote)
    if a == b:
        return "exact"
    if a and b and (a.startswith(b) or b.startswith(a)):
        return "prefix"
    return "neither"


def main():
    base_path, variant_path, db_path, audit_path = sys.argv[1:5]
    base, variant = read(base_path), read(variant_path)
    lost = sorted(set(base) - set(variant))
    switched = sorted(r for r in set(base) & set(variant) if base[r][:2] != variant[r][:2])
    new = sorted(set(variant) - set(base))
    print(f"base accepted={len(base)} variant accepted={len(variant)}")
    print(f"lost={len(lost)} switched={len(switched)} new={len(new)}")
    for label, ids in (("LOST", lost), ("SWITCHED", switched)):
        for release in ids[:40]:
            print(f"  {label} #{release}: base={base[release][:2]} variant={variant.get(release, ('-', '-'))[:2]}")

    db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    rows = []
    for release in new:
        local = db.execute(
            "SELECT album_artist, album_title FROM local_releases WHERE id=?", (release,)
        ).fetchone()
        remote = db.execute(
            "SELECT artist, title, release_url FROM release_matches WHERE local_release_id=?",
            (release,),
        ).fetchone()
        provider, _, scope, score, version = variant[release]
        rows.append(
            (release, provider, relation(local[1], remote[1]), score, scope,
             (local[0] or "").strip(), (local[1] or "").strip(),
             remote[0] or "", remote[1] or "", remote[2])
        )
    print("new by provider:", dict(Counter(r[1] for r in rows)))
    print("new by album-title relation:", dict(Counter(r[2] for r in rows)))

    random.seed(11)
    neither = [r for r in rows if r[2] == "neither"]
    rest = [r for r in rows if r[2] != "neither"]
    sample = neither + random.sample(rest, min(30, len(rest)))
    with open(audit_path, "w", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t")
        writer.writerow(["local_id", "provider", "title_relation", "score", "scope",
                         "local_artist", "local_title", "remote_artist", "remote_title", "url"])
        writer.writerows(sample)
    print(f"audit sheet: {audit_path} ({len(neither)} neither + {len(sample) - len(neither)} sampled)")


main()
