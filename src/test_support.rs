//! Helpers shared by unit tests: scratch directories and audio fixtures.

use crate::matcher::{
    self, ArtistKind, Candidate, DiscoveryEvidence, EmbeddedReleaseEvidence, LocalRelease,
    LocalTrack, RemoteRelease, RemoteTrack, TrackEntryType,
};
use crate::providers::{ProviderKind, ReleaseId};
use crate::scanner::{ObservedMusicBrainzIds, ScannedTrack};
use crate::store::{self, StoredCandidate};
use sqlx::SqlitePool;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// A scratch directory removed when dropped, including when a test panics.
pub(crate) struct ScratchDir(PathBuf);

impl ScratchDir {
    pub(crate) fn new(label: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "radish-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Canonical, so paths compare equal to what the importer stores.
        Self(dir.canonicalize().unwrap())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes a minimal valid PCM WAV file.
pub(crate) fn write_wav(path: &Path) {
    let data = [0u8; 32];
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&44100u32.to_le_bytes());
    bytes.extend_from_slice(&88200u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&data);
    std::fs::write(path, bytes).unwrap();
}

/// Writes a `mbdump.tar.bz2` containing the given table files, as the real
/// MusicBrainz core dump is laid out: one tar entry per table in COPY format.
pub(crate) fn write_mb_archive(dir: &Path, files: &[(&str, &str)]) -> PathBuf {
    let path = dir.join("mbdump.tar.bz2");
    let file = std::fs::File::create(&path).unwrap();
    let encoder = bzip2::write::BzEncoder::new(file, bzip2::Compression::best());
    let mut builder = tar::Builder::new(encoder);

    for (name, contents) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("mbdump/{name}"), contents.as_bytes())
            .unwrap();
    }

    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap().flush().unwrap();
    path
}

/// Writes about one second of silent MP3 under an ID3v2.3 tag that holds
/// `frames` exactly as given: `(frame ID, Latin-1 text, frame flags)`.
///
/// Written by hand rather than through Lofty, so a test can store values
/// (`199x` in a year frame) and flags (encryption) that Lofty would refuse.
pub(crate) fn write_id3v23_mp3(path: &Path, frames: &[(&str, &str, u16)]) {
    write_mp3(path, &id3v23_tag(&id3v23_frames(frames)));
}

/// ID3v2.3 frames holding `(frame ID, Latin-1 text, frame flags)`.
pub(crate) fn id3v23_frames(frames: &[(&str, &str, u16)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (id, text, flags) in frames {
        let content = [&[0u8][..], text.as_bytes()].concat();
        bytes.extend_from_slice(id.as_bytes());
        bytes.extend_from_slice(&u32::try_from(content.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(&flags.to_be_bytes());
        bytes.extend_from_slice(&content);
    }
    bytes
}

/// ID3v2.2 frames holding `(frame ID, Latin-1 text)`: each a three-character
/// ID, a three-byte size and the text, with no flags.
pub(crate) fn id3v22_frames(frames: &[(&str, &str)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (id, text) in frames {
        let content = [&[0u8][..], text.as_bytes()].concat();
        bytes.extend_from_slice(id.as_bytes());
        bytes.extend_from_slice(&u32::try_from(content.len()).unwrap().to_be_bytes()[1..]);
        bytes.extend_from_slice(&content);
    }
    bytes
}

/// A tag header that says ID3v2.3, with no flags, followed by `body`.
pub(crate) fn id3v23_tag(body: &[u8]) -> Vec<u8> {
    // The tag size is synchsafe: seven bits per byte.
    let size = u32::try_from(body.len()).unwrap();
    let mut bytes = b"ID3\x03\x00\x00".to_vec();
    bytes.extend(
        (0..4)
            .rev()
            .map(|shift| ((size >> (7 * shift)) & 0x7f) as u8),
    );
    bytes.extend_from_slice(body);
    bytes
}

/// Writes `tag` followed by about one second of silent MP3.
pub(crate) fn write_mp3(path: &Path, tag: &[u8]) {
    let mut bytes = tag.to_vec();
    // MPEG-1 Layer III at 128 kbit/s and 44.1 kHz: 417 bytes per frame, and
    // 40 frames last about a second.
    for _ in 0..40 {
        bytes.extend_from_slice(&[0xff, 0xfb, 0x90, 0x00]);
        bytes.extend_from_slice(&[0u8; 413]);
    }
    std::fs::write(path, bytes).unwrap();
}

/// Appends an ID3v1.1 tag holding `[title, artist, album, year]` and `track`.
///
/// Each field is padded with NUL bytes to its fixed width, as in the files
/// found in the wild; a test can pass trailing spaces for taggers that pad
/// with those.
pub(crate) fn append_id3v1(path: &Path, fields: [&str; 4], track: u8) {
    let [title, artist, album, year] = fields;
    let mut tag = b"TAG".to_vec();
    for (text, width) in [(title, 30), (artist, 30), (album, 30), (year, 4)] {
        let start = tag.len();
        tag.extend_from_slice(text.as_bytes());
        tag.resize(start + width, 0);
    }
    // A 28-byte comment, the NUL that marks ID3v1.1, the track, no genre.
    tag.resize(tag.len() + 29, 0);
    tag.extend_from_slice(&[track, 0xff]);
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(&tag).unwrap();
}

/// An APEv2 tag, with no header, holding `(key, value)` text items.
///
/// Each value is stored exactly as given, so a test can store text that is
/// not the UTF-8 the format requires.
pub(crate) fn ape_tag(items: &[(&str, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (key, value) in items {
        bytes.extend_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
        // Flags 0: a text item that may be changed.
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(value);
    }
    // The footer: version 2000, the size of the items and the footer, the
    // item count, flags 0 (no header) and eight reserved bytes.
    let size = u32::try_from(bytes.len() + 32).unwrap();
    bytes.extend_from_slice(b"APETAGEX");
    for field in [2000, size, u32::try_from(items.len()).unwrap(), 0] {
        bytes.extend_from_slice(&field.to_le_bytes());
    }
    bytes.extend_from_slice(&[0; 8]);
    bytes
}

/// Appends an APEv2 tag holding `(key, value)` text items, see [`ape_tag`].
pub(crate) fn append_ape(path: &Path, items: &[(&str, &[u8])]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(&ape_tag(items)).unwrap();
}

/// Writes a WAV file whose tag holds `album` and the release `year`.
pub(crate) fn write_tagged_wav(path: &Path, album: &str, year: u16) {
    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::items::Timestamp;
    use lofty::tag::{Accessor, Tag};

    write_wav(path);
    let mut file = lofty::probe::Probe::open(path).unwrap().read().unwrap();
    let mut tag = Tag::new(file.primary_tag_type());
    tag.set_album(album.to_string());
    tag.set_date(Timestamp {
        year,
        ..Timestamp::default()
    });
    file.insert_tag(tag);
    file.save_to_path(path, WriteOptions::default()).unwrap();
}

/// The length of every fixture track, in seconds, local and remote alike.
pub(crate) const FIXTURE_SECS: f64 = 180.0;

/// A valid MBID for fixtures.
pub(crate) const FIXTURE_MBID: &str = "76df3287-6cda-33eb-8e9a-044b5e15ffdd";

/// A second valid MBID, for a second candidate.
pub(crate) const OTHER_MBID: &str = "11111111-1111-1111-1111-111111111111";

/// The MusicBrainz release ID of `mbid`.
pub(crate) fn mbid_release(mbid: &str) -> ReleaseId {
    ReleaseId::MusicBrainz(mbid.parse().unwrap())
}

/// A local release of `titles` in `dir`, as matching sees it, without a database.
pub(crate) fn local_release(id: i64, dir: &str, album: &str, titles: &[&str]) -> LocalRelease {
    LocalRelease {
        id,
        directory: dir.to_string(),
        album_title: Some(album.to_string()),
        album_artist: Some("Artist".to_string()),
        artist_kind: ArtistKind::AlbumArtist,
        year: Some(2001),
        barcode: None,
        catalog_number: None,
        embedded_discogs_id: None,
        embedded_evidence: EmbeddedReleaseEvidence::default(),
        is_compilation: false,
        has_album_tag: true,
        reads_directory: true,
        revision: "rev".to_string(),
        tracks: titles
            .iter()
            .enumerate()
            .map(|(index, title)| LocalTrack {
                id: index as i64 + 1,
                path: format!("{dir}/{:02}.flac", index + 1),
                title: Some((*title).to_string()),
                disc: None,
                track_number: Some(index as i64 + 1),
                duration_secs: Some(FIXTURE_SECS),
                release_track_id: None,
                recording_id: None,
                identifier_problems: Vec::new(),
            })
            .collect(),
    }
}

/// Stores a release of `titles` in `dir` and returns it as matching loads it.
pub(crate) async fn seed_release(
    pool: &SqlitePool,
    dir: &str,
    album: &str,
    titles: &[&str],
) -> LocalRelease {
    let directory_id = store::upsert_directory(pool, Path::new(dir)).await.unwrap();
    for (index, title) in titles.iter().enumerate() {
        let number = index as i64 + 1;
        let track = ScannedTrack {
            path: Path::new(dir).join(format!("{number:02}.flac")),
            title: Some((*title).to_string()),
            artist: Some("Artist".to_string()),
            album: Some(album.to_string()),
            album_artist: Some("Artist".to_string()),
            year: Some(2001),
            track_number: Some(number),
            disc_number: None,
            barcode: None,
            catalog_number: None,
            discogs_release_id: None,
            musicbrainz: ObservedMusicBrainzIds::default(),
            duration_secs: Some(FIXTURE_SECS),
            modified_at: 1,
        };
        store::upsert_track(pool, &track, directory_id)
            .await
            .unwrap();
    }
    store::rebuild_local_releases(pool, None).await.unwrap();
    store::load_local_releases(pool, None)
        .await
        .unwrap()
        .into_iter()
        .find(|release| release.directory == dir)
        .unwrap()
}

/// A provider release of `titles` under `id`, each track [`FIXTURE_SECS`] long.
pub(crate) fn remote_release(id: ReleaseId, album: &str, titles: &[&str]) -> RemoteRelease {
    let url = format!("https://example.invalid/release/{}", id.storage_id());
    RemoteRelease {
        id,
        group_id: None,
        title: album.to_string(),
        artists: vec!["Artist".to_string()],
        year: Some(2001),
        country: Some("US".to_string()),
        labels: vec!["Label".to_string()],
        catalog_numbers: vec!["CAT-1".to_string()],
        formats: vec!["CD".to_string()],
        barcodes: Vec::new(),
        tracklist: titles
            .iter()
            .enumerate()
            .map(|(index, title)| RemoteTrack {
                position: Some((index + 1).to_string()),
                title: (*title).to_string(),
                artists: Vec::new(),
                duration_secs: Some(FIXTURE_SECS),
                entry_type: TrackEntryType::Track,
                medium_position: None,
                track_position: None,
                release_track_id: None,
                recording_id: None,
                artist_credit: None,
            })
            .collect(),
        url,
        lookup_aliases: Vec::new(),
        artist_credit: None,
        structure_issues: Vec::new(),
        edition_notes: Vec::new(),
        provider_metadata: serde_json::Value::Null,
    }
}

/// Queues `release` for `remote`'s provider with `remote` as its one candidate,
/// evaluated as a direct selection, under the release's current guard.
pub(crate) async fn queue(pool: &SqlitePool, release: &LocalRelease, remote: RemoteRelease) {
    let candidate = Candidate::new(release, remote);
    let evaluation = matcher::evaluate(release, &candidate, None, &DiscoveryEvidence::default());
    let guard = store::load_match_guard(pool, release.id).await.unwrap();
    let stored = StoredCandidate {
        release: candidate.release.clone(),
        mapping: candidate.mapping.clone(),
        score: candidate.score,
        auto_accept: evaluation.auto_accept,
        match_scope: evaluation.match_scope,
        blockers: evaluation.blockers.clone(),
        warnings: evaluation.warnings.clone(),
        evidence: evaluation.evidence.clone(),
    };
    store::queue_review(
        pool,
        release.id,
        candidate.release.provider(),
        &guard.source_revision,
        guard.accepted_match_id,
        &[stored],
        &candidate.mapping,
        &["uncertain".to_string()],
    )
    .await
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixtures_seed_a_release_and_queue_it() {
        let pool = crate::db::connect_in_memory().await.unwrap();
        let release = seed_release(&pool, "/music/a", "Blue", &["One", "Two"]).await;
        assert_eq!(release.directory, "/music/a");
        assert_eq!(release.tracks.len(), 2);

        let remote = remote_release(mbid_release(FIXTURE_MBID), "Blue", &["One", "Two"]);
        queue(&pool, &release, remote).await;
        let review = store::load_review(&pool, release.id, ProviderKind::MusicBrainz)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(review.candidates.len(), 1);
        assert!(review.candidates[0].mapping.complete);

        let pure = local_release(7, "/music/b", "Red", &["One"]);
        assert_eq!(pure.tracks[0].path, "/music/b/01.flac");
        assert_eq!(
            mbid_release(OTHER_MBID).provider(),
            ProviderKind::MusicBrainz
        );
    }
}
