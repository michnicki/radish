#!/usr/bin/env python3
"""Compare two `match --offline` runs made by measure-matching.sh.

usage: compare-matching.py [--slice NAME]... [--sample N] [--seed N]
                           BASE.accepted.tsv VARIANT.accepted.tsv VARIANT.db AUDIT.tsv

Prints lost, switched and new accepts. New accepts are split by provider and by
how the local album title relates to the remote one (exact, prefix, neither).
AUDIT.tsv lists every new accept whose titles are neither equal nor a prefix of
one another, plus a seeded random sample (--sample, default 30) of the rest, for
reading by hand.

Audit slices: the rule that accepted a release is not stored, so each matching
rule gets a slice that selects the new accepts it could have affected. Pass
--slice once per slice (see SLICES below). With any --slice, AUDIT.tsv holds
the slice rows instead of the default sheet: for each slice every new accept in
it, or, when there are more than 3 * --sample, all those with title relation
"neither" plus a seeded random --sample of the rest (--seed, default 11). The
first column is the slice, and columns that only some slices fill are empty
elsewhere. A per-slice count line is printed. Two slices add rows after their
own: no-lengths adds the 30 accepts with the fewest local tracks (marked
"fewest-tracks" in the reason column) and disc appends a section listing
sibling disc folders with their remote ids side by side.

    compare-matching.py --slice junk-titles --slice disc \\
        base.accepted.tsv new.accepted.tsv new.db audit.tsv
"""
import argparse
import csv
import json
import random
import re
import sqlite3
import unicodedata
from collections import Counter

AUDIO_EXTENSION = re.compile(
    r"\.(mp3|flac|ogg|m4a|wav|wma|aac|ape|mpc|opus|wv|aiff|aif)\s*$", re.IGNORECASE
)
# "01 - X", "01-X", "03. X", "01_X", "01) X" and "(01) X", but not "4.0" or "10.000".
NUMBER_PREFIX = re.compile(r"^\s*(\(\d{1,3}\)|\d{1,3}\s*[-._)](?!\d))\s*\S")
RELEASE_TAG = re.compile(r"[-_]([0-9A-Za-z]{2,8})\s*$")
STEREO = re.compile(r"\bstereo\b", re.IGNORECASE)
DISC_FOLDER = re.compile(r"^(cd|disc|disk)[ _.-]?\d{1,3}$", re.IGNORECASE)
PLACEHOLDERS = {"self titled", "selftitled", "s t", "st", "same", "eponymous",
                "unknown title", "unknown album", "untitled"}
WEAK_SCORE = 0.90
LOCAL_COLUMNS = ["slice", "local_id", "provider", "title_relation", "score", "scope",
                 "local_artist", "local_title", "remote_artist", "remote_title", "url"]
EXTRA_COLUMNS = ["reason", "weak_pairs", "lowest_pair", "unknown_lengths", "formats",
                 "dirs", "medium_position"]


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


def norm_track(text):
    """Track-title normal form: NFKD, no combining marks, casefolded, `&` as `and`."""
    text = unicodedata.normalize("NFKD", text or "")
    text = "".join(ch for ch in text if not unicodedata.combining(ch)).casefold()
    text = re.sub(r"['‘’ʼ`´]", "", text).replace("&", " and ")
    text = "".join(ch if ch.isalnum() else " " for ch in text)
    return " ".join(text.split())


def similarity(a, b):
    """1 - Levenshtein distance / longer length, after track-title normalisation."""
    a, b = norm_track(a), norm_track(b)
    if not a and not b:
        return 1.0
    previous = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        current = [i]
        for j, cb in enumerate(b, 1):
            current.append(min(previous[j] + 1, current[j - 1] + 1, previous[j - 1] + (ca != cb)))
        previous = current
    return 1 - previous[-1] / max(len(a), len(b))


def path_parts(directory):
    return [part for part in (directory or "").replace("\\", "/").split("/") if part]


def load_release(db, release, has_medium):
    """Everything a slice may look at for one local release and its accepted match."""
    local = db.execute(
        "SELECT album_artist, album_title, directory_path FROM local_releases WHERE id=?",
        (release,),
    ).fetchone()
    medium = "medium_position" if has_medium else "NULL"
    match = db.execute(
        f"SELECT id, artist, title, release_url, metadata_json, {medium} "
        "FROM release_matches WHERE local_release_id=?",
        (release,),
    ).fetchone()
    titles = [title or "" for (title,) in db.execute(
        "SELECT title FROM tracks WHERE local_release_id=? ORDER BY disc_number, track_number, id",
        (release,),
    )]
    pairs = db.execute(
        "SELECT t.title, m.remote_title FROM release_track_matches m "
        "JOIN tracks t ON t.id = m.track_id WHERE m.match_id=? ORDER BY m.remote_ordinal",
        (match[0],),
    ).fetchall()
    return {
        "local_artist": (local[0] or "").strip(), "local_title": (local[1] or "").strip(),
        "directory": local[2] or "", "remote_artist": match[1] or "",
        "remote_title": match[2] or "", "url": match[3], "snapshot": json.loads(match[4]),
        "medium_position": match[5], "titles": titles, "pairs": pairs,
    }


def junk_titles(r):
    titles = r["titles"]
    for title in titles:
        if NUMBER_PREFIX.match(title):
            return {"reason": f"number prefix: {title}"}
        if AUDIO_EXTENSION.search(title):
            return {"reason": f"audio extension: {title}"}
    found = [RELEASE_TAG.search(title) for title in titles]
    tags = {m.group(1).casefold() for m in found if m}
    if len(titles) >= 3 and all(found) and len(tags) == 1:
        return {"reason": f"common suffix: -{tags.pop()} ({titles[0]})"}
    return None


def stereo(r):
    snapshot = r["snapshot"]
    for field in ("formats", "edition_notes"):
        for text in snapshot.get(field) or []:
            if STEREO.search(text):
                return {"reason": f"{field}: {text}", "formats": "; ".join(snapshot.get("formats") or [])}
    return None


def weak_track_titles(r):
    scored = [(similarity(local, remote), local or "", remote or "") for local, remote in r["pairs"]]
    weak = [item for item in scored if item[0] < WEAK_SCORE]
    if not weak:
        return None
    score, local, remote = min(weak)
    return {"weak_pairs": f"{len(weak)}/{len(scored)}",
            "lowest_pair": f"{local} -> {remote} ({score:.2f})"}


def tracklist_lengths(snapshot):
    """(unknown, total) over the remote entries that are tracks."""
    tracks = [e for e in snapshot.get("tracklist") or [] if e.get("entry_type", "track") == "track"]
    return sum(1 for e in tracks if e.get("duration_secs") is None), len(tracks)


def no_lengths(r):
    unknown, total = tracklist_lengths(r["snapshot"])
    if not unknown:
        return None
    return {"unknown_lengths": f"{unknown}/{total}", "formats": "; ".join(r["snapshot"].get("formats") or [])}


def disc(r):
    parts = path_parts(r["directory"])
    last = parts[-1] if parts else ""
    if not DISC_FOLDER.match(last) and r["medium_position"] is None:
        return None
    return {"dirs": "/".join(parts[-2:]), "medium_position": r["medium_position"]}


def placeholder(r):
    title = r["local_title"]
    if title in ("", "."):
        return {"reason": f"empty or dot title: {title!r}"}
    if norm(title) in PLACEHOLDERS:
        return {"reason": f"placeholder title: {title}"}
    return None


SLICES = {
    "junk-titles": junk_titles,
    "stereo": stereo,
    "weak-track-titles": weak_track_titles,
    "no-lengths": no_lengths,
    "disc": disc,
    "placeholder": placeholder,
}


def pick(hits, sample, rng):
    """All hits, or every "neither" one plus `sample` random others when there are many."""
    if len(hits) <= 3 * sample:
        return hits
    neither = [h for h in hits if h[0][2] == "neither"]
    rest = [h for h in hits if h[0][2] != "neither"]
    return neither + rng.sample(rest, min(sample, len(rest)))


def sibling_discs(db, variant):
    """Variant accepts in disc folders grouped by parent directory, with their remote ids."""
    groups = {}
    for release, directory in db.execute("SELECT id, directory_path FROM local_releases"):
        parts = path_parts(directory)
        if release in variant and parts and DISC_FOLDER.match(parts[-1]):
            groups.setdefault("/".join(parts[:-1]).casefold(), []).append((release, parts))
    out = []
    for members in groups.values():
        if len(members) < 2:
            continue
        members.sort(key=lambda m: m[1][-1].casefold())
        cells = [f"{parts[-1]} #{release} {variant[release][0]}:{variant[release][1]}"
                 for release, parts in members]
        same = len({variant[release][:2] for release, _ in members}) == 1
        out.append(("same" if same else "DIFFERENT", "/".join(members[0][1][:-1]), cells))
    out.sort(key=lambda g: (g[0] == "same", g[1].casefold()))
    return out


def write_slices(db, variant, rows, slices, sample, seed, audit_path):
    has_medium = any(c[1] == "medium_position" for c in db.execute("PRAGMA table_info(release_matches)"))
    loaded = {row[0]: load_release(db, row[0], has_medium) for row in rows}
    out = []
    for name in slices:
        rng = random.Random(f"{seed}/{name}")
        hits = []
        for row in rows:
            extra = SLICES[name](loaded[row[0]])
            if extra is not None:
                hits.append((row, extra))
        written = pick(hits, sample, rng)
        if name == "no-lengths":
            seen = {row[0] for row, _ in written}
            fewest = sorted(hits, key=lambda h: (len(loaded[h[0][0]]["titles"]), h[0][0]))[:30]
            for row, extra in fewest:
                if row[0] not in seen:
                    count = len(loaded[row[0]]["titles"])
                    written.append((row, {**extra, "reason": f"fewest-tracks ({count} local)"}))
        out.extend((name, row, extra) for row, extra in written)
        print(f"slice {name}: {len(hits)} new accepts in slice, {len(written)} written")

    siblings = sibling_discs(db, variant) if "disc" in slices else []
    with open(audit_path, "w", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t")
        writer.writerow(LOCAL_COLUMNS + EXTRA_COLUMNS)
        for name, row, extra in out:
            writer.writerow([name, *row] + [extra.get(column, "") for column in EXTRA_COLUMNS])
        if siblings:
            writer.writerow([])
            writer.writerow(["disc-siblings", "same_release", "parent_directory", "discs (dir #local_id provider:remote_id)"])
            for verdict, parent, cells in siblings:
                writer.writerow(["disc-siblings", verdict, parent, *cells])
    if siblings:
        different = sum(1 for g in siblings if g[0] != "same")
        print(f"disc siblings: {len(siblings)} groups of disc folders, {different} on different releases")
    print(f"audit sheet: {audit_path} ({len(out)} slice rows)")


def main():
    parser = argparse.ArgumentParser(description="Compare two match --offline runs.")
    parser.add_argument("base")
    parser.add_argument("variant")
    parser.add_argument("db")
    parser.add_argument("audit")
    parser.add_argument("--slice", action="append", default=[], choices=sorted(SLICES),
                        help="audit slice of the new accepts (repeatable)")
    parser.add_argument("--sample", type=int, default=30, help="sample size (default 30)")
    parser.add_argument("--seed", type=int, default=11, help="sampling seed (default 11)")
    args = parser.parse_args()
    base, variant = read(args.base), read(args.variant)
    lost = sorted(set(base) - set(variant))
    switched = sorted(r for r in set(base) & set(variant) if base[r][:2] != variant[r][:2])
    new = sorted(set(variant) - set(base))
    print(f"base accepted={len(base)} variant accepted={len(variant)}")
    print(f"lost={len(lost)} switched={len(switched)} new={len(new)}")
    for label, ids in (("LOST", lost), ("SWITCHED", switched)):
        for release in ids[:40]:
            print(f"  {label} #{release}: base={base[release][:2]} variant={variant.get(release, ('-', '-'))[:2]}")

    db = sqlite3.connect(f"file:{args.db}?mode=ro", uri=True)
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

    if args.slice:
        slices = list(dict.fromkeys(args.slice))
        write_slices(db, variant, rows, slices, args.sample, args.seed, args.audit)
        return

    random.seed(args.seed)
    neither = [r for r in rows if r[2] == "neither"]
    rest = [r for r in rows if r[2] != "neither"]
    sample = neither + random.sample(rest, min(args.sample, len(rest)))
    with open(args.audit, "w", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t")
        writer.writerow(["local_id", "provider", "title_relation", "score", "scope",
                         "local_artist", "local_title", "remote_artist", "remote_title", "url"])
        writer.writerows(sample)
    print(f"audit sheet: {args.audit} ({len(neither)} neither + {len(sample) - len(neither)} sampled)")


main()
