# MusicBrainz fixtures

These JSON documents are synthetic. They are not copies of any real
MusicBrainz response and describe no real release: the MBIDs, titles, artists,
labels, and barcodes were invented for offline tests.

- `release-single.json`: one CD medium, two tracks, complete credits and labels.
- `release-multidisc.json`: two media, one repeated recording, partial date.

Automated tests never contact MusicBrainz or Discogs. Each fixture is served by
a loopback HTTP server, or decoded directly in unit tests.
