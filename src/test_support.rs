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
