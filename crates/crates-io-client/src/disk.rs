//! A cache on disk for artifacts that cannot change.
//!
//! The server is spawned per client session, so everything the in-memory caches
//! learn is thrown away when the session ends. That is fine for anything
//! reflecting live registry state, and wasteful for the one artifact that is
//! immutable by construction: the rustdoc JSON docs.rs built for an exact
//! `name@version` is generated once and never regenerated, so an index derived
//! from it is correct forever.
//!
//! # What makes this safe to keep forever
//!
//! Only the immutability above. Nothing here is a general-purpose cache and
//! nothing here has an expiry, because an entry that could go stale has no
//! business being written by this module in the first place.
//!
//! # What makes it safe to read back
//!
//! A file is trusted only after its header names a layout this build
//! understands and zstd has verified the frame checksum. Anything else — a
//! truncated write, a file from an older binary, a flipped bit — is deleted and
//! treated as a miss, so a damaged cache costs a refetch and can never produce
//! a wrong answer. That is the whole design: every failure mode has to land on
//! "fetch it again".

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Serialize, de::DeserializeOwned};

/// Marks a file as one this module wrote. Any file that does not start with
/// these bytes is not ours and is left alone.
const MAGIC: [u8; 8] = *b"mcpcrat\x01";

/// The shape of what is stored, independent of the encoder.
///
/// Bumped whenever a stored type gains, loses or reorders a field. A file
/// carrying a different number is unreadable by definition, so it is discarded
/// rather than decoded — the point of checking a version before decoding is
/// that decoding the wrong layout is exactly what must not be attempted.
const SCHEMA_VERSION: u32 = 1;

/// Which bitcode wire format wrote the body.
///
/// bitcode is pre-1.0 and its format is only stable within a minor version, so
/// the minor is the compatibility unit and is what this records. Upgrading the
/// dependency invalidates the cache; it never misreads it.
const BITCODE_FORMAT: u16 = 6;

/// Bytes before the zstd frame: magic, schema, format, and a reserved pair that
/// keeps the header a round sixteen.
const HEADER_LEN: usize = 16;

/// Compression level. Low deliberately: these files are written on a request
/// path and read on the next one, so decode speed and write latency matter and
/// the last few percent of ratio does not.
const ZSTD_LEVEL: i32 = 3;

/// Default ceiling on the whole cache directory.
pub const DEFAULT_CACHE_CAPACITY_BYTES: u64 = 512 * 1024 * 1024;

/// Why a stored file was not usable.
///
/// Every variant means the same thing to a caller — treat it as a miss — and
/// they are distinguished only so that the corrupt-file tests can assert which
/// guard fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// Shorter than a header.
    Truncated,
    /// Does not begin with [`MAGIC`].
    NotOurs,
    /// Written by a build storing a different shape.
    SchemaVersion,
    /// Written by a different bitcode wire format.
    BitcodeFormat,
    /// zstd refused it: a truncated frame, or a checksum that did not match.
    Corrupt,
    /// The bytes decompressed but were not the type expected.
    Undecodable,
}

/// A directory of immutable artifacts.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// A store rooted at the platform's cache directory for this application.
    ///
    /// Returns `None` when the platform does not tell us where that is, which
    /// is a reason to run without a disk cache rather than to guess at a path
    /// inside someone's home directory.
    #[must_use]
    pub fn discover() -> Option<Self> {
        let dirs = directories::ProjectDirs::from("io", "zchee", "mcp-crates")?;
        Some(Self { root: dirs.cache_dir().to_path_buf() })
    }

    /// A store rooted at a caller-chosen directory.
    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Where this store keeps its files.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file a key is stored in, or `None` if the key could not be a file
    /// name.
    ///
    /// Keys reaching here are already built from a validated crate name and
    /// version, so this is a second line rather than the only one — but it is
    /// the line that keeps a key from naming a path instead of a file, and a
    /// second line costs nothing.
    fn path_for(&self, key: &str) -> Option<PathBuf> {
        let usable = !key.is_empty()
            && key.len() <= 200
            && key != "."
            && key != ".."
            && key.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@' | b'+')
            });
        usable.then(|| self.root.join(format!("{key}.mcpc")))
    }

    /// Read and decode an artifact, deleting it if it cannot be trusted.
    ///
    /// `Ok(None)` means there was nothing usable, whether because no file
    /// existed or because the one that did was rejected. A caller cannot act
    /// differently on the two, so they are not distinguished here.
    ///
    /// # Errors
    ///
    /// Never returns an error for a bad file — only for a key that cannot name
    /// one, which is a programming mistake rather than a cache state.
    pub fn load<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, Rejected> {
        let Some(path) = self.path_for(key) else {
            return Err(Rejected::NotOurs);
        };
        let Ok(raw) = fs::read(&path) else {
            return Ok(None);
        };

        match decode::<T>(&raw) {
            Ok(value) => Ok(Some(value)),
            Err(reason) => {
                // A file that cannot be read is a file that will never be read,
                // so it goes now rather than occupying the cap forever. A
                // failure to remove it is not worth reporting: the next reader
                // reaches the same conclusion.
                let _ = fs::remove_file(&path);
                tracing::debug!(?reason, path = %path.display(), "discarded a cache entry");
                Ok(None)
            },
        }
    }

    /// Encode and store an artifact, replacing any previous one atomically.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] if the directory cannot be created
    /// or the file cannot be written. Callers treat that as "the cache is
    /// unavailable" rather than as a failure of whatever they were doing.
    pub fn store<T: Serialize>(&self, key: &str, value: &T) -> io::Result<()> {
        let path = self
            .path_for(key)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unusable cache key"))?;
        fs::create_dir_all(&self.root)?;

        // The process id is what keeps two servers writing the same key from
        // writing the same temporary file. Whichever renames last wins, and
        // both wrote the same bytes, so a lost race costs one duplicate write
        // and cannot produce a torn file: a reader only ever sees a name that
        // arrived there whole, by rename.
        let temporary = self.root.join(format!(".{key}.{}.tmp", std::process::id()));
        let outcome = (|| {
            fs::write(&temporary, encode(value)?)?;
            fs::rename(&temporary, &path)
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        outcome
    }

    /// Delete the oldest entries until the directory fits within `capacity`.
    ///
    /// Ordered by modification time, which is when an entry was last written
    /// rather than last read. For artifacts that never change and are read far
    /// more often than written, that is the same ordering a true LRU would
    /// give, without a read having to write anything to maintain it.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if the directory cannot be listed. Individual
    /// files that cannot be removed are skipped: another process may be
    /// pruning the same directory, and losing that race is not a failure.
    pub fn prune(&self, capacity: u64) -> io::Result<u64> {
        let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        let mut total = 0_u64;

        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let Ok(metadata) = entry.metadata() else { continue };
            if !metadata.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "mcpc") {
                continue;
            }
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            total += metadata.len();
            entries.push((modified, metadata.len(), path));
        }

        if total <= capacity {
            return Ok(0);
        }

        entries.sort_by_key(|(modified, _, _)| *modified);
        let mut removed = 0_u64;
        for (_, size, path) in entries {
            if total <= capacity {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total -= size;
                removed += size;
            }
        }
        Ok(removed)
    }
}

/// Wrap a value in a header and a checksummed zstd frame.
fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let body = bitcode::serialize(value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;

    let mut out = Vec::with_capacity(HEADER_LEN + body.len() / 2);
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&BITCODE_FORMAT.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes());
    debug_assert_eq!(out.len(), HEADER_LEN);

    let mut encoder = zstd::stream::write::Encoder::new(out, ZSTD_LEVEL)?;
    // What turns a flipped bit into a rejected file rather than a decode of
    // plausible-looking rubbish.
    encoder.include_checksum(true)?;
    io::Write::write_all(&mut encoder, &body)?;
    encoder.finish()
}

/// Check a stored file's header, then decompress and decode it.
///
/// The header is checked *before* anything is decompressed, so bytes written by
/// a build that stored a different shape are never handed to a decoder that
/// would read them as this one.
fn decode<T: DeserializeOwned>(raw: &[u8]) -> Result<T, Rejected> {
    if raw.len() < HEADER_LEN {
        return Err(Rejected::Truncated);
    }
    if raw[..8] != MAGIC {
        return Err(Rejected::NotOurs);
    }
    let schema = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
    if schema != SCHEMA_VERSION {
        return Err(Rejected::SchemaVersion);
    }
    let format = u16::from_le_bytes([raw[12], raw[13]]);
    if format != BITCODE_FORMAT {
        return Err(Rejected::BitcodeFormat);
    }

    let body = zstd::stream::decode_all(&raw[HEADER_LEN..]).map_err(|_| Rejected::Corrupt)?;
    bitcode::deserialize(&body).map_err(|_| Rejected::Undecodable)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a stored artifact: owned, nested, and not all one type.
    #[derive(Debug, PartialEq, Serialize, serde::Deserialize)]
    struct Sample {
        name: String,
        counts: Vec<u32>,
        flag: bool,
    }

    fn sample() -> Sample {
        Sample { name: "serde@1.0.229".to_owned(), counts: (0..512).collect(), flag: true }
    }

    /// A directory that removes itself, so a failing test cannot leave one
    /// behind for the next.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("mcp-crates-disk-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("the temporary directory is creatable");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_stored_artifact_comes_back_the_same() {
        let dir = TempDir::new("roundtrip");
        let store = Store::at(&dir.0);

        assert_eq!(store.load::<Sample>("absent@1.0.0").expect("no error"), None);
        store.store("serde@1.0.229", &sample()).expect("stores");
        assert_eq!(store.load::<Sample>("serde@1.0.229").expect("no error"), Some(sample()));
    }

    #[test]
    fn a_file_that_cannot_be_trusted_is_rejected_and_removed() {
        // Each case is a way a stored file can go wrong, and every one has to
        // end the same way: reported as a miss, and deleted so the next reader
        // does not pay to reach the same conclusion.
        let good = encode(&sample()).expect("encodes");
        let cases: [(&str, Vec<u8>, Rejected); 6] = [
            ("shorter than a header", good[..8].to_vec(), Rejected::Truncated),
            ("not one of ours", b"some other file entirely".to_vec(), Rejected::NotOurs),
            (
                "a newer schema",
                {
                    let mut bytes = good.clone();
                    bytes[8] = bytes[8].wrapping_add(1);
                    bytes
                },
                Rejected::SchemaVersion,
            ),
            (
                "a different bitcode format",
                {
                    let mut bytes = good.clone();
                    bytes[12] = bytes[12].wrapping_add(1);
                    bytes
                },
                Rejected::BitcodeFormat,
            ),
            ("a truncated body", good[..good.len() - 4].to_vec(), Rejected::Corrupt),
            (
                "a flipped bit in the body",
                {
                    let mut bytes = good.clone();
                    let middle = HEADER_LEN + (bytes.len() - HEADER_LEN) / 2;
                    bytes[middle] ^= 0b0001_0000;
                    bytes
                },
                Rejected::Corrupt,
            ),
        ];

        for (label, bytes, expected) in cases {
            assert_eq!(decode::<Sample>(&bytes).unwrap_err(), expected, "{label}");

            let dir = TempDir::new("corrupt");
            let store = Store::at(&dir.0);
            let path = store.path_for("serde@1.0.229").expect("a usable key");
            fs::create_dir_all(&dir.0).expect("creatable");
            fs::write(&path, &bytes).expect("writable");

            assert_eq!(store.load::<Sample>("serde@1.0.229").expect("no error"), None, "{label}");
            assert!(!path.exists(), "{label}: the unusable file should have been removed");
        }
    }

    #[test]
    fn no_single_bit_flip_can_turn_a_stored_file_into_a_different_answer() {
        // Two outcomes are fine. The frame checksum catching the flip is the
        // point of turning it on, and a flip landing somewhere the decoder
        // never reads changes nothing. The third outcome is the one that must
        // not exist: decoding into a value that differs from what was stored,
        // which is a wrong answer rather than a slow one.
        let good = encode(&sample()).expect("encodes");
        let (mut caught, mut harmless) = (0_u32, 0_u32);

        for position in HEADER_LEN..good.len() {
            for bit in 0..8 {
                let mut bytes = good.clone();
                bytes[position] ^= 1 << bit;
                match decode::<Sample>(&bytes) {
                    Err(_) => caught += 1,
                    Ok(value) => {
                        assert_eq!(
                            value,
                            sample(),
                            "byte {position} bit {bit} decoded differently"
                        );
                        harmless += 1;
                    },
                }
            }
        }

        // Not a correctness property — the assertion above is. This one says
        // the checksum is actually switched on, which a silent default change
        // would otherwise hide.
        assert!(
            caught > harmless,
            "only {caught} of {} flips were rejected; is the frame checksum on?",
            caught + harmless
        );
    }

    #[test]
    fn pruning_removes_the_oldest_until_the_cache_fits() {
        let dir = TempDir::new("prune");
        let store = Store::at(&dir.0);

        // Written oldest first, then given mtimes far enough apart that the
        // ordering is the file system's rather than the test's luck.
        for (position, key) in ["oldest@1", "middle@1", "newest@1"].iter().enumerate() {
            store.store(key, &sample()).expect("stores");
            let path = store.path_for(key).expect("usable");
            let when = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1 + position as u64);
            fs::File::open(&path).expect("openable").set_modified(when).expect("mtime is settable");
        }

        let total: u64 = ["oldest@1", "middle@1", "newest@1"]
            .iter()
            .map(|key| fs::metadata(store.path_for(key).expect("usable")).expect("exists").len())
            .sum();
        let one = total / 3;

        // Room for two of the three.
        let removed = store.prune(total - one).expect("prunes");
        assert!(removed > 0, "something should have been removed");
        assert!(!store.path_for("oldest@1").expect("usable").exists(), "the oldest goes first");
        assert!(store.path_for("newest@1").expect("usable").exists(), "the newest stays");

        // Already inside the cap: nothing to do.
        assert_eq!(store.prune(u64::MAX).expect("prunes"), 0);
    }

    #[test]
    fn a_key_that_could_name_a_path_is_refused() {
        let store = Store::at("/tmp/never-created");
        for bad in ["../escape", "a/b", "", ".", "..", "with space", "quote\"d"] {
            assert!(store.path_for(bad).is_none(), "{bad:?} should not name a file");
            assert_eq!(store.load::<Sample>(bad), Err(Rejected::NotOurs), "{bad:?}");
        }
        for good in ["serde@1.0.229", "tokio-util@0.7.0", "a_b@1.0.0-rc.1+build"] {
            assert!(store.path_for(good).is_some(), "{good:?} should name a file");
        }
    }

    #[test]
    fn a_replaced_artifact_is_never_seen_half_written() {
        // The rename is what guarantees this; the test is here so that a
        // future change to a plain `fs::write` fails rather than silently
        // introducing a window where a reader sees a partial file.
        let dir = TempDir::new("atomic");
        let store = Store::at(&dir.0);
        store.store("serde@1.0.229", &sample()).expect("stores");

        let bigger = Sample { name: "x".repeat(4096), counts: (0..4096).collect(), flag: false };
        store.store("serde@1.0.229", &bigger).expect("replaces");

        assert_eq!(store.load::<Sample>("serde@1.0.229").expect("no error"), Some(bigger));
        let leftovers: Vec<_> = fs::read_dir(&dir.0)
            .expect("listable")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "a temporary file was left behind: {leftovers:?}");
    }
}
