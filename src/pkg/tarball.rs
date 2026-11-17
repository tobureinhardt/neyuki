//! Package archives: gzip-compressed ustar files.
//!
//! Packing is deterministic (sorted paths, zero times and owners, fixed
//! modes), so packing the same files twice gives the same checksum.
//! Unpacking accepts only regular files and directories with plain relative
//! paths: links, devices, absolute paths and `..` are refused, and the
//! unpacked size is capped so a small archive cannot fill the disk.

use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;

const BLOCK: usize = 512;
/// The most an archive may unpack to.
pub const MAX_UNPACKED: u64 = 200 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;

fn write_octal(field: &mut [u8], value: u64) {
    // `len - 1` digits, then NUL.
    let digits = field.len() - 1;
    let text = format!("{:0width$o}", value, width = digits);
    field[..digits].copy_from_slice(text.as_bytes());
    field[digits] = 0;
}

/// Splits a path into ustar's `prefix` (≤ 155 bytes) and `name` (≤ 100).
fn split_path(path: &str) -> Result<(&str, &str), String> {
    if path.len() <= 100 {
        return Ok(("", path));
    }
    for (index, _) in path.match_indices('/') {
        let (prefix, name) = (&path[..index], &path[index + 1..]);
        if prefix.len() <= 155 && name.len() <= 100 && !name.is_empty() {
            return Ok((prefix, name));
        }
    }
    Err(format!("the path '{}' is too long for a package", path))
}

fn header(path: &str, size: u64, directory: bool) -> Result<[u8; BLOCK], String> {
    let (prefix, name) = split_path(path)?;
    let mut block = [0u8; BLOCK];
    block[..name.len()].copy_from_slice(name.as_bytes());
    write_octal(&mut block[100..108], if directory { 0o755 } else { 0o644 });
    write_octal(&mut block[108..116], 0);
    write_octal(&mut block[116..124], 0);
    write_octal(&mut block[124..136], size);
    write_octal(&mut block[136..148], 0);
    block[156] = if directory { b'5' } else { b'0' };
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    block[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    block[148..156].fill(b' ');
    let sum: u64 = block.iter().map(|&b| b as u64).sum();
    let text = format!("{:06o}\0 ", sum);
    block[148..156].copy_from_slice(text.as_bytes());
    Ok(block)
}

/// Packs `(path, contents)` pairs, paths `/`-separated and relative, into a
/// `.tgz`.
pub fn pack(files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let mut sorted: Vec<&(String, Vec<u8>)> = files.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut tar = Vec::new();
    for (path, contents) in sorted {
        check_path(path)?;
        tar.extend_from_slice(&header(path, contents.len() as u64, false)?);
        tar.extend_from_slice(contents);
        let padding = (BLOCK - contents.len() % BLOCK) % BLOCK;
        tar.resize(tar.len() + padding, 0);
    }
    tar.extend_from_slice(&[0u8; BLOCK * 2]);

    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder
        .write_all(&tar)
        .and_then(|_| encoder.finish())
        .map_err(|e| format!("cannot compress the package: {}", e))
}

/// A path is safe to unpack when it is relative and has only plain parts.
fn check_path(path: &str) -> Result<(), String> {
    let refuse = || Err(format!("refusing the unsafe path '{}' in a package", path));
    if path.is_empty() || path.contains('\\') || path.contains('\0') || path.contains(':') {
        return refuse();
    }
    let parsed = Path::new(path);
    if parsed.is_absolute() || path.starts_with('/') {
        return refuse();
    }
    for component in parsed.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => return refuse(),
        }
    }
    Ok(())
}

fn parse_octal(field: &[u8]) -> Result<u64, String> {
    let text: String = field
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as char)
        .collect();
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).map_err(|_| "a package archive has a corrupt header".to_string())
}

fn field_str(field: &[u8]) -> Result<String, String> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8(field[..end].to_vec())
        .map_err(|_| "a package archive has a path that is not UTF-8".to_string())
}

/// The files in a `.tgz`, as `(path, contents)`, after the checks above.
pub fn unpack_to_memory(archive: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut decoder = GzDecoder::new(archive).take(MAX_UNPACKED + 1);
    let mut tar = Vec::new();
    decoder
        .read_to_end(&mut tar)
        .map_err(|e| format!("the package archive is not valid gzip: {}", e))?;
    if tar.len() as u64 > MAX_UNPACKED {
        return Err("the package unpacks to more than 200 MB".to_string());
    }

    let mut files = Vec::new();
    let mut offset = 0;
    while offset + BLOCK <= tar.len() {
        let block = &tar[offset..offset + BLOCK];
        if block.iter().all(|&b| b == 0) {
            break;
        }
        let stored = parse_octal(&block[148..156])?;
        let mut copy = [0u8; BLOCK];
        copy.copy_from_slice(block);
        copy[148..156].fill(b' ');
        let sum: u64 = copy.iter().map(|&b| b as u64).sum();
        if sum != stored {
            return Err("a package archive has a corrupt header (bad checksum)".to_string());
        }

        let name = field_str(&block[..100])?;
        let prefix = if &block[257..262] == b"ustar" {
            field_str(&block[345..500])?
        } else {
            String::new()
        };
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{}/{}", prefix, name)
        };
        let size = parse_octal(&block[124..136])?;
        let kind = block[156];
        let data_start = offset + BLOCK;
        let data_end = data_start
            .checked_add(size as usize)
            .filter(|&end| end <= tar.len())
            .ok_or("a package archive is truncated")?;
        offset = data_start + (size as usize).div_ceil(BLOCK) * BLOCK;

        match kind {
            b'0' | 0 => {
                check_path(&path)?;
                files.push((
                    path.trim_start_matches("./").to_string(),
                    tar[data_start..data_end].to_vec(),
                ));
                if files.len() > MAX_ENTRIES {
                    return Err("the package has too many files".to_string());
                }
            }
            b'5' => check_path(path.trim_end_matches('/'))?,
            // Extended headers from other tools; their contents are not needed.
            b'x' | b'g' => {}
            _ => {
                return Err(format!(
                    "the package contains '{}', which is not a regular file \
                     (links and devices are not allowed)",
                    path
                ));
            }
        }
    }
    Ok(files)
}

/// Unpacks a `.tgz` into `dest`, which must not exist yet.
pub fn unpack(archive: &[u8], dest: &Path) -> Result<(), String> {
    let files = unpack_to_memory(archive)?;
    fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {}", dest.display(), e))?;
    for (path, contents) in files {
        let target = dest.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
        }
        fs::write(&target, contents)
            .map_err(|e| format!("cannot write {}: {}", target.display(), e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_deterministic() {
        let long = format!(
            "{}/{}.nyk",
            "deep/".repeat(30).trim_end_matches('/'),
            "x".repeat(60)
        );
        let files = vec![
            ("init.nyk".to_string(), b"return {}".to_vec()),
            ("lib/util.nyk".to_string(), vec![7u8; 1500]),
            (long.clone(), b"long".to_vec()),
        ];
        let a = pack(&files).unwrap();
        let mut reversed = files.clone();
        reversed.reverse();
        assert_eq!(a, pack(&reversed).unwrap(), "packing ignores input order");

        let mut back = unpack_to_memory(&a).unwrap();
        back.sort();
        let mut expected = files;
        expected.sort();
        assert_eq!(back, expected);
    }

    #[test]
    fn refuses_unsafe_paths() {
        for path in ["../evil.nyk", "/etc/passwd", "a/../../b", "C:/x", "a\\b"] {
            assert!(pack(&[(path.to_string(), vec![])]).is_err(), "{}", path);
        }
        // Built by hand, since `pack` refuses it: a symlink entry.
        let mut tar = header("link", 0, false).unwrap();
        tar[156] = b'2';
        tar[148..156].fill(b' ');
        let sum: u64 = tar.iter().map(|&b| b as u64).sum();
        tar[148..156].copy_from_slice(format!("{:06o}\0 ", sum).as_bytes());
        let mut raw = tar.to_vec();
        raw.extend_from_slice(&[0u8; BLOCK * 2]);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&raw).unwrap();
        let archive = encoder.finish().unwrap();
        assert!(
            unpack_to_memory(&archive)
                .unwrap_err()
                .contains("not a regular file")
        );
    }
}
