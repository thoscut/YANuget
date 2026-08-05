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

    // Flags(2), then Streams(2) — the number of stream headers. Every offset
    // below comes from the file itself, so all of this arithmetic is checked:
    // a crafted PDB must not be able to overflow an index and panic the server.
    let _flags = read_u16(bytes, pos)?;
    let stream_count = read_u16(bytes, pos.checked_add(2)?)? as usize;
    pos = pos.checked_add(4)?;

    // --- Stream headers (II.24.2.2) ---
    for _ in 0..stream_count {
        let offset = read_u32(bytes, pos)? as usize;
        let _size = read_u32(bytes, pos.checked_add(4)?)?;
        pos = pos.checked_add(8)?;

        // Name: ASCII, NUL-terminated, padded to the next 4-byte boundary, at
        // most 32 bytes including the terminator.
        let (name, name_len) = read_stream_name(bytes, pos)?;
        pos = pos.checked_add(name_len)?;

        if name == "#Pdb" {
            // The PDB id is the first 20 bytes of the stream; the GUID is the
            // leading 16.
            let guid = bytes.get(offset..offset.checked_add(16)?)?;
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
    let b = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(4)?)?;
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

    /// Every offset here comes from the file, so a crafted PDB must fail to
    /// parse rather than panic — a panic in a push handler is a free DoS.
    #[test]
    fn hostile_pdbs_return_none_instead_of_panicking() {
        let good = make_portable_pdb(&[1u8; 16]);

        // Truncation at every length must be handled. Some prefixes still hold
        // a complete GUID and legitimately parse; the requirement is that none
        // of them panics.
        for len in 0..good.len() {
            let _ = portable_pdb_signature(&good[..len]);
        }
        // A file cut before the GUID cannot yield a key.
        assert!(portable_pdb_signature(&good[..good.len() - 8]).is_none());

        // A version length that runs past the end of the file.
        let mut huge_version = good.clone();
        huge_version[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(portable_pdb_signature(&huge_version).is_none());

        // A stream count far larger than the headers actually present. Here the
        // first header still is `#Pdb`, so the key is found before the bogus
        // count matters — what must not happen is walking off the end.
        let mut many_streams = good.clone();
        let count_at = 16 + 12 + 2; // header + version + flags
        many_streams[count_at..count_at + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(portable_pdb_signature(&many_streams).is_some());

        // ...and with the name changed so the walk actually runs to the end of
        // that inflated count, it must terminate with None rather than panic.
        let mut many_streams_no_pdb = many_streams.clone();
        let name_at = 16 + 12 + 4 + 8;
        many_streams_no_pdb[name_at..name_at + 4].copy_from_slice(b"#Str");
        assert!(portable_pdb_signature(&many_streams_no_pdb).is_none());

        // A `#Pdb` stream whose offset points past the end of the file.
        let mut bad_offset = good.clone();
        let header_at = 16 + 12 + 4; // header + version + flags + streams
        bad_offset[header_at..header_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(portable_pdb_signature(&bad_offset).is_none());

        // An unterminated stream name.
        let mut no_terminator = good.clone();
        let name_at = header_at + 8;
        for b in &mut no_terminator[name_at..name_at + 8] {
            *b = b'A';
        }
        assert!(portable_pdb_signature(&no_terminator).is_none());
    }
}
