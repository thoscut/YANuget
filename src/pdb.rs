//! Extracting the symbol-server lookup key from a Portable PDB.
//!
//! A symbol server indexes each `.pdb` by an **SSQP key** (Simple Symbol Query
//! Protocol) so a debugger can fetch it with a single `GET`. For a *Portable*
//! PDB — the format the modern .NET tooling (`dotnet pack --include-symbols`,
//! `MSBuild` with `DebugType=portable`) produces — the key is derived from the
//! 20-byte *PDB id* that lives at the start of the `#Pdb` metadata stream.
//!
//! The key is `{guid}{age}` where `guid` is the 16-byte GUID formatted as 32
//! upper-case hex digits in the canonical GUID byte order, and `age` is the
//! literal `FFFFFFFF` for Portable PDBs (per the SSQP conventions).
//!
//! Parsing only walks the small ECMA-335 *metadata root* header (II.24.2.1) and
//! the stream-header table (II.24.2.2); it never reads the (potentially large)
//! debugging tables that follow.
//!
//! Native (Windows / MSF) PDBs use a different on-disk container and are not
//! parsed here — [`portable_pdb_signature`] returns `None` for them so the
//! caller can skip indexing rather than fail the whole push.

/// The ECMA-335 metadata root signature, ASCII `"BSJB"`, little-endian.
const METADATA_SIGNATURE: u32 = 0x424A_5342;

/// Compute the SSQP symbol key for a Portable PDB given its raw bytes.
///
/// Returns `None` if the bytes are not a parseable Portable PDB (e.g. a native
/// PDB, or a truncated/corrupt file).
pub fn portable_pdb_signature(bytes: &[u8]) -> Option<String> {
    let guid = portable_pdb_guid(bytes)?;
    Some(format!("{}FFFFFFFF", guid_to_hex(&guid)))
}

/// Locate the 16-byte GUID at the start of the `#Pdb` stream.
fn portable_pdb_guid(bytes: &[u8]) -> Option<[u8; 16]> {
    // --- Metadata root header (II.24.2.1) ---
    if read_u32(bytes, 0)? != METADATA_SIGNATURE {
        return None;
    }
    // 4: MajorVersion(2) 6: MinorVersion(2) 8: Reserved(4) 12: Length(4)
    let version_len = read_u32(bytes, 12)? as usize;
    // The version string is padded to a 4-byte boundary; Length already is per
    // the spec, but be defensive and round up anyway.
    let version_padded = version_len.div_ceil(4) * 4;
    let mut pos = 16usize.checked_add(version_padded)?;

    // Flags(2), then Streams(2) — the number of stream headers.
    let _flags = read_u16(bytes, pos)?;
    let stream_count = read_u16(bytes, pos + 2)? as usize;
    pos = pos.checked_add(4)?;

    // --- Stream headers (II.24.2.2) ---
    for _ in 0..stream_count {
        let offset = read_u32(bytes, pos)? as usize;
        let _size = read_u32(bytes, pos + 4)?;
        pos = pos.checked_add(8)?;

        // Name: ASCII, NUL-terminated, padded to the next 4-byte boundary, at
        // most 32 bytes including the terminator.
        let (name, name_len) = read_stream_name(bytes, pos)?;
        pos = pos.checked_add(name_len)?;

        if name == "#Pdb" {
            // The PDB id is the first 20 bytes of the stream; the GUID is the
            // leading 16.
            let guid = bytes.get(offset..offset + 16)?;
            return guid.try_into().ok();
        }
    }
    None
}

/// Read a NUL-terminated, 4-byte-aligned stream name, returning the name and the
/// number of bytes it occupies (including terminator and padding).
fn read_stream_name(bytes: &[u8], start: usize) -> Option<(String, usize)> {
    // Names are capped at 32 bytes by the spec.
    for len in 1..=32 {
        let idx = start.checked_add(len - 1)?;
        if *bytes.get(idx)? == 0 {
            let name = std::str::from_utf8(&bytes[start..idx]).ok()?.to_string();
            // Round the consumed length (name bytes + NUL) up to 4 bytes.
            let consumed = len.div_ceil(4) * 4;
            return Some((name, consumed));
        }
    }
    None
}

/// Format a raw 16-byte GUID as 32 upper-case hex digits in canonical order:
/// the first three components (4, 2 and 2 bytes) are stored little-endian and
/// therefore byte-reversed; the trailing 8 bytes are kept as-is.
fn guid_to_hex(g: &[u8; 16]) -> String {
    let order = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut s = String::with_capacity(32);
    for &i in &order {
        s.push_str(&format!("{:02X}", g[i]));
    }
    s
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let b = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal Portable PDB blob containing just the metadata root and a
    /// single `#Pdb` stream whose first 16 bytes are `guid`.
    fn make_portable_pdb(guid: &[u8; 16]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&METADATA_SIGNATURE.to_le_bytes()); // signature
        buf.extend_from_slice(&1u16.to_le_bytes()); // major
        buf.extend_from_slice(&1u16.to_le_bytes()); // minor
        buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
        let version = b"PDB v1.0\0\0\0\0"; // length must be multiple of 4
        buf.extend_from_slice(&(version.len() as u32).to_le_bytes());
        buf.extend_from_slice(version);
        buf.extend_from_slice(&0u16.to_le_bytes()); // flags
        buf.extend_from_slice(&1u16.to_le_bytes()); // one stream

        // Reserve the stream-header slot; we patch the offset once we know it.
        let header_pos = buf.len();
        buf.extend_from_slice(&0u32.to_le_bytes()); // offset (patched below)
        buf.extend_from_slice(&20u32.to_le_bytes()); // size
        buf.extend_from_slice(b"#Pdb\0\0\0\0"); // name, padded to 8 bytes

        let stream_offset = buf.len() as u32;
        buf[header_pos..header_pos + 4].copy_from_slice(&stream_offset.to_le_bytes());

        buf.extend_from_slice(guid); // GUID (16)
        buf.extend_from_slice(&[0u8; 4]); // stamp (4) -> 20-byte PDB id
        buf
    }

    #[test]
    fn extracts_signature_from_portable_pdb() {
        // Bytes 0..16 of the PDB id.
        let guid: [u8; 16] = [
            0xF6, 0x72, 0x7B, 0x49, 0x0A, 0x39, 0xFC, 0x44, 0x87, 0x8E, 0x5A, 0x2D, 0x63, 0xB6,
            0xCC, 0x4B,
        ];
        let pdb = make_portable_pdb(&guid);
        let key = portable_pdb_signature(&pdb).unwrap();
        // Data1/Data2/Data3 are byte-reversed; Data4 kept as-is; +FFFFFFFF.
        assert_eq!(key, "497B72F6390A44FC878E5A2D63B6CC4BFFFFFFFF");
    }

    #[test]
    fn rejects_non_portable_pdb() {
        assert!(portable_pdb_signature(b"Microsoft C/C++ MSF 7.00\r\n").is_none());
        assert!(portable_pdb_signature(b"").is_none());
        assert!(portable_pdb_signature(b"not a pdb").is_none());
    }
}
