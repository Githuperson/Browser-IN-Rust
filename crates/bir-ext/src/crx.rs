//! CRX3 / zip packaging.
//!
//! A `.crx` file is:
//!
//! ```text
//! "Cr24" magic (4 bytes)
//! version           u32 LE   (3 for CRX3)
//! header_length     u32 LE
//! header            protobuf `CrxFileHeader`
//! <the rest>        a plain zip archive
//! ```
//!
//! To derive the extension ID we need the developer's public key, which lives in the
//! protobuf header as `sha256_with_rsa.public_key` (field 2, sub-field 1). Rather than
//! add a protobuf dependency for one message, the header is scanned with a ~40-line
//! varint reader. If the key cannot be found we fall back to hashing the archive, which
//! produces a stable (but device-specific) ID — the same thing Chrome does for
//! unpacked extensions.

use std::{
  io::{Cursor, Read, Write},
  path::Path,
};

use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::Error;

pub const CRX_MAGIC: &[u8; 4] = b"Cr24";

pub struct CrxInfo {
  pub version: u32,
  pub header_len: u32,
  /// Byte offset at which the zip archive begins.
  pub zip_offset: usize,
  /// Extension id derived from the developer public key, when present.
  pub extension_id: Option<String>,
}

impl CrxInfo {
  pub fn zip_body<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
    &bytes[self.zip_offset.min(bytes.len())..]
  }
}

/// Inspect a CRX file. Returns `Err` if it is not a CRX3 we can understand.
pub fn parse_crx(bytes: &[u8]) -> Result<CrxInfo, Error> {
  if bytes.len() < 12 {
    return Err(Error::Extension("file is too small to be a CRX".into()));
  }
  if &bytes[0..4] != CRX_MAGIC {
    return Err(Error::Extension("not a CRX file (bad magic)".into()));
  }
  let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
  if version != 3 {
    return Err(Error::Extension(format!(
      "CRX version {version} is not supported (only CRX3 is)"
    )));
  }
  let header_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
  let header_end = 12 + header_len;
  if header_end > bytes.len() {
    return Err(Error::Extension("CRX header overruns the file".into()));
  }
  let header = &bytes[12..header_end];
  let extension_id = find_public_key(header).map(extension_id_from_key);

  Ok(CrxInfo {
    version,
    header_len: header_len as u32,
    zip_offset: header_end,
    extension_id,
  })
}

/// True for a plain zip file (what most `.zip` extension bundles are).
pub fn is_zip(bytes: &[u8]) -> bool {
  bytes.len() >= 4 && bytes[0] == b'P' && bytes[1] == b'K' && bytes[2] == 3 && bytes[3] == 4
}

/// Chrome's extension id: the first 16 bytes of SHA-256, each byte rendered as two
/// letters from `a`–`p`.
pub fn extension_id_from_key(public_key: &[u8]) -> String {
  let digest = Sha256::digest(public_key);
  id_from_hash(&digest[..16])
}

/// Id for an unpacked directory: hash the canonical path, same rendering.
///
/// This matches Chrome's behaviour closely enough that an extension unpacked in the
/// same directory gets the same id across reinstalls, which matters because the id is
/// what storage, `runtime.getURL` and content-script matching are keyed on.
pub fn id_from_path(path: &Path) -> String {
  let canonical = path
    .canonicalize()
    .unwrap_or_else(|_| path.to_path_buf());
  let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
  id_from_hash(&digest[..16])
}

fn id_from_hash(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len() * 2);
  for byte in bytes {
    out.push((b'a' + (byte >> 4)) as char);
    out.push((b'a' + (byte & 0x0f)) as char);
  }
  out
}

/// Extract a CRX body or zip archive into `dest`.
///
/// Every entry is resolved through `enclosed_name()`, which rejects `../` components —
/// a malicious extension must not be able to write outside its own directory.
pub fn unpack_package(archive: &[u8], dest: &Path) -> Result<(), Error> {
  let mut zip = ZipArchive::new(Cursor::new(archive))?;
  std::fs::create_dir_all(dest)?;

  for index in 0..zip.len() {
    let mut entry = zip.by_index(index)?;
    let Some(target) = entry.enclosed_name() else {
      // Zip slip attempt (or an absolute path): skip it rather than fail the install.
      continue;
    };
    let out_path = dest.join(target);
    if entry.is_dir() {
      std::fs::create_dir_all(&out_path)?;
      continue;
    }
    if let Some(parent) = out_path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(&out_path)?;
    let mut buffer = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buffer)?;
    file.write_all(&buffer)?;
  }
  Ok(())
}

/// Read a protobuf varint. Returns `(value, next_index)`.
fn read_varint(buf: &[u8], mut index: usize) -> Option<(u64, usize)> {
  let mut value: u64 = 0;
  let mut shift = 0u32;
  loop {
    let byte = *buf.get(index)?;
    index += 1;
    value |= ((byte & 0x7f) as u64) << shift;
    if byte & 0x80 == 0 {
      return Some((value, index));
    }
    shift += 7;
    if shift > 63 {
      return None;
    }
  }
}

/// Walk a `CrxFileHeader` looking for an `AsymmetricKeyProof.public_key`.
fn find_public_key(header: &[u8]) -> Option<&[u8]> {
  let mut index = 0usize;
  while index < header.len() {
    let (tag, next) = read_varint(header, index)?;
    index = next;
    let field = tag >> 3;
    match tag & 7 {
      0 => {
        let (_, next) = read_varint(header, index)?;
        index = next;
      }
      1 => index += 8,
      2 => {
        let (len, next) = read_varint(header, index)?;
        let end = next + len as usize;
        if end > header.len() {
          return None;
        }
        let payload = &header[next..end];
        // 2 = sha256_with_rsa, 3 = sha256_with_ecdsa; both carry public_key = 1.
        if field == 2 || field == 3 {
          if let Some(key) = find_field_bytes(payload, 1) {
            return Some(key);
          }
        }
        index = end;
      }
      5 => index += 4,
      _ => return None,
    }
  }
  None
}

/// Find field `wanted` (wire type 2) inside one protobuf message.
fn find_field_bytes(message: &[u8], wanted: u64) -> Option<&[u8]> {
  let mut index = 0usize;
  while index < message.len() {
    let (tag, next) = read_varint(message, index)?;
    index = next;
    let field = tag >> 3;
    match tag & 7 {
      0 => {
        let (_, next) = read_varint(message, index)?;
        index = next;
      }
      1 => index += 8,
      2 => {
        let (len, next) = read_varint(message, index)?;
        let end = next + len as usize;
        if end > message.len() {
          return None;
        }
        if field == wanted {
          return Some(&message[next..end]);
        }
        index = end;
      }
      5 => index += 4,
      _ => return None,
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ids_are_32_chars_of_a_to_p() {
    let id = extension_id_from_key(b"not-a-real-key");
    assert_eq!(id.len(), 32);
    assert!(id.chars().all(|c| ('a'..='p').contains(&c)));
    // Deterministic.
    assert_eq!(id, extension_id_from_key(b"not-a-real-key"));
    assert_ne!(id, extension_id_from_key(b"different-key"));
  }

  #[test]
  fn rejects_non_crx() {
    assert!(parse_crx(b"hello world, not a crx").is_err());
    assert!(parse_crx(b"").is_err());
  }

  #[test]
  fn parses_a_minimal_crx3() {
    // Header: field 2 (len 4) { field 1 (len 2) = 0xAA 0xBB }
    let header: Vec<u8> = vec![0x12, 0x04, 0x0a, 0x02, 0xaa, 0xbb];
    let mut bytes = Vec::new();
    bytes.extend_from_slice(CRX_MAGIC);
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(&(header.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(b"PK\x03\x04rest-of-zip");

    let info = parse_crx(&bytes).unwrap();
    assert_eq!(info.version, 3);
    assert_eq!(info.zip_offset, 12 + header.len());
    assert!(info.extension_id.is_some());
    assert_eq!(info.zip_body(&bytes), b"PK\x03\x04rest-of-zip");
  }

  #[test]
  fn glob_and_zip_detection() {
    assert!(is_zip(b"PK\x03\x04...."));
    assert!(!is_zip(b"Cr24...."));
  }
}
