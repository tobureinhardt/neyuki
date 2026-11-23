//! The bundle format: an index of files and app metadata followed by the
//! file contents, appended to a copy of the runtime executable.
//!
//! ```text
//! payload  b"NYKPAK\0\x01"
//!          u32 count, then count × (str key, str value)   app metadata
//!          u32 count, then count × (str path, u64 size)   files, in order
//!          the file contents, back to back
//! trailer  u64 payload size, b"NYKAPP\0\x01"              executables only
//! ```
//!
//! Integers are little-endian and a `str` is a `u32` byte length followed by
//! UTF-8. The trailer sits at the very end of the executable, so the runtime
//! finds the payload by reading the last 16 bytes of its own file. Nothing
//! but the index is read at startup; file contents are read when asked for.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Starts the payload; the last byte is the format version.
pub const PAYLOAD_MAGIC: &[u8; 8] = b"NYKPAK\0\x01";
/// Ends an executable that carries a payload.
pub const TRAILER_MAGIC: &[u8; 8] = b"NYKAPP\0\x01";
/// Payload size plus magic.
pub const TRAILER_LEN: u64 = 16;

/// Caps that keep a damaged index from allocating without bound.
const MAX_STRING: u32 = 64 * 1024;
const MAX_ENTRIES: u32 = 1 << 20;

/// Normalizes a path inside a bundle: `\` and `/` both separate, empty and
/// `.` components are dropped, so `./assets//logo.png` and
/// `assets\logo.png` both become `assets/logo.png`. Returns `None` for paths
/// that cannot name a bundled file: empty ones, absolute ones (`/x`, `C:x`,
/// `\\server`), and any with a `..` component or a NUL byte.
pub fn normalize(path: &str) -> Option<String> {
    if path.is_empty() || path.contains('\0') || path.starts_with(['/', '\\']) {
        return None;
    }
    let mut parts = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            // A drive prefix (`C:`) or an alternate data stream (`a:b`).
            _ if part.contains(':') => return None,
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// What a path names inside a bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    File { size: u64 },
    Dir,
}

/// Where the file contents live.
#[derive(Debug)]
enum Source {
    Memory(Arc<[u8]>),
    /// `base` is the offset of the first file's contents in `path`.
    File {
        path: PathBuf,
        base: u64,
    },
}

/// A parsed bundle: metadata, the file index and where to read contents.
#[derive(Debug)]
pub struct Bundle {
    meta: BTreeMap<String, String>,
    /// Normalized path to (offset from the first file, size).
    files: BTreeMap<String, (u64, u64)>,
    dirs: BTreeSet<String>,
    source: Source,
}

/// Builds a payload. Paths must already be normalized and unique.
pub fn pack(
    meta: &BTreeMap<String, String>,
    files: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    let mut seen = BTreeSet::new();
    for (path, _) in files {
        if normalize(path).as_deref() != Some(path.as_str()) {
            return Err(format!("invalid path in bundle: '{}'", path));
        }
        if !seen.insert(path.as_str()) {
            return Err(format!("'{}' is in the bundle twice", path));
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(PAYLOAD_MAGIC);
    write_count(&mut out, meta.len())?;
    for (key, value) in meta {
        write_str(&mut out, key)?;
        write_str(&mut out, value)?;
    }
    write_count(&mut out, files.len())?;
    for (path, data) in files {
        write_str(&mut out, path)?;
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    }
    for (_, data) in files {
        out.extend_from_slice(data);
    }
    Ok(out)
}

fn write_count(out: &mut Vec<u8>, count: usize) -> Result<(), String> {
    match u32::try_from(count) {
        Ok(n) if n <= MAX_ENTRIES => {
            out.extend_from_slice(&n.to_le_bytes());
            Ok(())
        }
        _ => Err(format!("a bundle holds at most {} entries", MAX_ENTRIES)),
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) -> Result<(), String> {
    match u32::try_from(s.len()) {
        Ok(n) if n <= MAX_STRING => {
            out.extend_from_slice(&n.to_le_bytes());
            out.extend_from_slice(s.as_bytes());
            Ok(())
        }
        _ => Err(format!(
            "'{}…' is too long for a bundle",
            s.chars().take(32).collect::<String>()
        )),
    }
}

/// The trailer that follows a payload of `payload_len` bytes.
pub fn trailer(payload_len: u64) -> [u8; TRAILER_LEN as usize] {
    let mut out = [0; TRAILER_LEN as usize];
    out[..8].copy_from_slice(&payload_len.to_le_bytes());
    out[8..].copy_from_slice(TRAILER_MAGIC);
    out
}

/// The length of `bytes` without a payload and trailer at its end, so a
/// runtime that already carries an app can be built on again.
pub fn strip_trailer(bytes: &[u8]) -> usize {
    let len = bytes.len() as u64;
    if len < TRAILER_LEN {
        return bytes.len();
    }
    let tail = &bytes[(len - TRAILER_LEN) as usize..];
    if &tail[8..] != TRAILER_MAGIC {
        return bytes.len();
    }
    let payload = u64::from_le_bytes(tail[..8].try_into().unwrap_or_default());
    match len
        .checked_sub(TRAILER_LEN)
        .and_then(|n| n.checked_sub(payload))
    {
        Some(start) => start as usize,
        None => bytes.len(),
    }
}

/// Reads an index through a budget of `limit` bytes, so a damaged length
/// fails cleanly instead of reading past the payload.
struct IndexReader<R> {
    reader: R,
    consumed: u64,
    limit: u64,
}

impl<R: Read> IndexReader<R> {
    fn bytes(&mut self, n: usize) -> Result<Vec<u8>, String> {
        self.consumed += n as u64;
        if self.consumed > self.limit {
            return Err("bundle index is truncated".to_string());
        }
        let mut buf = vec![0; n];
        self.reader
            .read_exact(&mut buf)
            .map_err(|err| format!("cannot read bundle: {}", err))?;
        Ok(buf)
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap_or_default()))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let bytes = self.bytes(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap_or_default()))
    }

    fn count(&mut self) -> Result<u32, String> {
        match self.u32()? {
            n if n <= MAX_ENTRIES => Ok(n),
            _ => Err("bundle index is damaged".to_string()),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let len = self.u32()?;
        if len > MAX_STRING {
            return Err("bundle index is damaged".to_string());
        }
        String::from_utf8(self.bytes(len as usize)?)
            .map_err(|_| "bundle index is damaged".to_string())
    }
}

/// A parsed index: metadata, path to (offset, size), and where the contents
/// start in the payload.
type Index = (BTreeMap<String, String>, BTreeMap<String, (u64, u64)>, u64);

/// Reads the payload index from `reader`, which holds `payload_len` bytes.
fn read_index<R: Read>(reader: R, payload_len: u64) -> Result<Index, String> {
    let mut input = IndexReader {
        reader,
        consumed: 0,
        limit: payload_len,
    };
    if input.bytes(PAYLOAD_MAGIC.len())? != PAYLOAD_MAGIC {
        return Err("not a Neyuki bundle (or one from another Neyuki version)".to_string());
    }
    let mut meta = BTreeMap::new();
    for _ in 0..input.count()? {
        let key = input.string()?;
        let value = input.string()?;
        meta.insert(key, value);
    }
    let file_count = input.count()?;
    let mut sizes = Vec::with_capacity(file_count.min(1024) as usize);
    for _ in 0..file_count {
        let path = input.string()?;
        let size = input.u64()?;
        sizes.push((path, size));
    }

    let base = input.consumed;
    let mut files = BTreeMap::new();
    let mut offset = 0u64;
    for (path, size) in sizes {
        if normalize(&path).as_deref() != Some(path.as_str()) {
            return Err(format!("bundle holds an invalid path: '{}'", path));
        }
        let end = offset
            .checked_add(size)
            .filter(|end| base.checked_add(*end).is_some_and(|e| e <= payload_len))
            .ok_or_else(|| "bundle contents are truncated".to_string())?;
        if files.insert(path.clone(), (offset, size)).is_some() {
            return Err(format!("bundle holds '{}' twice", path));
        }
        offset = end;
    }
    Ok((meta, files, base))
}

impl Bundle {
    /// Parses a payload held in memory (the output of [`pack`]).
    pub fn from_payload(payload: impl Into<Arc<[u8]>>) -> Result<Self, String> {
        let payload: Arc<[u8]> = payload.into();
        let (meta, files, base) = read_index(&payload[..], payload.len() as u64)?;
        let contents: Arc<[u8]> = payload[base as usize..].into();
        Ok(Self::new(meta, files, Source::Memory(contents)))
    }

    /// Opens a payload file on disk, such as the one inside a macOS `.app`.
    pub fn from_payload_file(path: &Path) -> Result<Self, String> {
        let file =
            File::open(path).map_err(|err| format!("cannot open {}: {}", path.display(), err))?;
        let len = file
            .metadata()
            .map_err(|err| format!("cannot open {}: {}", path.display(), err))?
            .len();
        let (meta, files, base) = read_index(BufReader::new(file), len)?;
        Ok(Self::new(
            meta,
            files,
            Source::File {
                path: path.to_path_buf(),
                base,
            },
        ))
    }

    /// Reads the payload appended to the executable at `path`, or returns
    /// `Ok(None)` when it carries none.
    pub fn from_executable(path: &Path) -> Result<Option<Self>, String> {
        let open_err = |err: io::Error| format!("cannot read {}: {}", path.display(), err);
        let mut file = File::open(path).map_err(open_err)?;
        let len = file.metadata().map_err(open_err)?.len();
        if len < TRAILER_LEN {
            return Ok(None);
        }
        let mut tail = [0; TRAILER_LEN as usize];
        file.seek(SeekFrom::Start(len - TRAILER_LEN))
            .map_err(open_err)?;
        file.read_exact(&mut tail).map_err(open_err)?;
        if &tail[8..] != TRAILER_MAGIC {
            return Ok(None);
        }
        let payload_len = u64::from_le_bytes(tail[..8].try_into().unwrap_or_default());
        let start = (len - TRAILER_LEN)
            .checked_sub(payload_len)
            .ok_or_else(|| format!("{} carries a damaged bundle", path.display()))?;
        file.seek(SeekFrom::Start(start)).map_err(open_err)?;
        let (meta, files, base) = read_index(BufReader::new(file), payload_len)?;
        Ok(Some(Self::new(
            meta,
            files,
            Source::File {
                path: path.to_path_buf(),
                base: start + base,
            },
        )))
    }

    fn new(
        meta: BTreeMap<String, String>,
        files: BTreeMap<String, (u64, u64)>,
        source: Source,
    ) -> Self {
        let mut dirs = BTreeSet::new();
        for path in files.keys() {
            let mut rest = path.as_str();
            while let Some((parent, _)) = rest.rsplit_once('/') {
                dirs.insert(parent.to_string());
                rest = parent;
            }
        }
        Self {
            meta,
            files,
            dirs,
            source,
        }
    }

    /// An app metadata value (`name`, `version`, `identifier`, `entry`, ...).
    pub fn meta(&self, key: &str) -> Option<&str> {
        self.meta.get(key).map(String::as_str)
    }

    /// Every app metadata entry, sorted by key.
    pub fn meta_entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.meta.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Every bundled file path, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// What `path` names in the bundle, if anything.
    pub fn entry(&self, path: &str) -> Option<Entry> {
        let path = normalize(path)?;
        if let Some(&(_, size)) = self.files.get(&path) {
            Some(Entry::File { size })
        } else if self.dirs.contains(&path) {
            Some(Entry::Dir)
        } else {
            None
        }
    }

    /// The contents of a bundled file, or `None` when it is not bundled.
    pub fn read(&self, path: &str) -> Option<io::Result<Vec<u8>>> {
        let &(offset, size) = self.files.get(&normalize(path)?)?;
        Some(match &self.source {
            Source::Memory(bytes) => Ok(bytes[offset as usize..(offset + size) as usize].to_vec()),
            Source::File { path, base } => (|| {
                let mut file = File::open(path)?;
                file.seek(SeekFrom::Start(base + offset))?;
                let mut buf = vec![0; usize::try_from(size).map_err(io::Error::other)?];
                file.read_exact(&mut buf)?;
                Ok(buf)
            })(),
        })
    }

    /// The names directly inside a bundled directory, sorted, or `None` when
    /// it is not one.
    pub fn list(&self, path: &str) -> Option<Vec<String>> {
        let path = normalize(path)?;
        if !self.dirs.contains(&path) {
            return None;
        }
        let prefix = format!("{}/", path);
        let mut names = BTreeSet::new();
        for candidate in self.files.keys().chain(self.dirs.iter()) {
            if let Some(rest) = candidate.strip_prefix(&prefix)
                && !rest.contains('/')
            {
                names.insert(rest.to_string());
            }
        }
        Some(names.into_iter().collect())
    }
}
