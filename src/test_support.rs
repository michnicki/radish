//! Helpers shared by unit tests: scratch directories and audio fixtures.

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
