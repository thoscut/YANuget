//! Reading the debug directory of a PE file (a .NET `.dll` or `.exe`).
//!
//! A symbol package's Portable PDB is only worth serving if it belongs to an
//! assembly in the package it claims to belong to, and the assembly says which
//! PDB that is: its debug directory carries a **CodeView** entry repeating the
//! PDB's GUID (with the PDB's stamp as the entry's time stamp) and, from any
//! compiler since Visual Studio 15.9, a **PdbChecksum** entry holding a hash of
//! the PDB itself. nuget.org validates symbol packages against both, and so
//! does [`crate::symbols`].
//!
//! Only headers and the few small records they point at are read: the DOS
//! header, the PE and optional headers, the section table, the debug directory
//! and each entry's data. The input is a forward-only stream (an entry being
//! decompressed out of a `.nupkg`), so reads go in file order and anything
//! behind the current position is left to a second pass. Every
//! offset and count comes from the file, so all of it is bounds-checked and
//! capped: a crafted assembly yields `None`, never a panic or an unbounded
//! read.

use std::io::{self, Read};

/// How far into an assembly any structure may lie. The debug directory of a
/// managed assembly sits near the end of `.text`, after the IL, metadata and
/// resources, so it can be tens of MiB in; this bounds how much decompression
/// one lookup can cost.
const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
/// Upper bounds on the tables walked; real assemblies have a handful of
/// sections and two to five debug entries.
const MAX_SECTIONS: usize = 96;
const MAX_DEBUG_ENTRIES: usize = 64;
/// Size of one `IMAGE_DEBUG_DIRECTORY`.
const DEBUG_ENTRY_SIZE: usize = 28;
/// `IMAGE_DEBUG_TYPE_CODEVIEW`.
const DEBUG_TYPE_CODEVIEW: u32 = 2;
/// The Portable PDB checksum entry type.
const DEBUG_TYPE_PDB_CHECKSUM: u32 = 19;
/// A CodeView entry describes a Portable PDB when its minor version is `PM`.
const PORTABLE_CODEVIEW_MINOR: u16 = 0x504D;

/// A CodeView (`RSDS`) debug entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeView {
    pub guid: [u8; 16],
    pub age: u32,
    /// The entry's time stamp; for a Portable PDB, the stamp half of its id.
    pub stamp: u32,
    /// Whether the entry declares a Portable (rather than Windows) PDB.
    pub portable: bool,
}

/// A `PdbChecksum` debug entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdbChecksum {
    /// The hash algorithm's name, e.g. `SHA256`.
    pub algorithm: String,
    pub checksum: Vec<u8>,
}

/// The debug entries of one PE file that matter for symbol validation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugDirectory {
    pub codeview: Vec<CodeView>,
    pub checksums: Vec<PdbChecksum>,
}

/// The outcome of one [`DebugDirectoryScan::pass`].
#[derive(Debug)]
pub enum Pass {
    /// The scan is complete: the directory, or `None` when the file is not a
    /// PE image or its headers do not hold together.
    Done(Option<DebugDirectory>),
    /// Some entry's data lies behind what this pass had already read; call
    /// `pass` once more with a fresh reader from the start of the file.
    Again,
}

/// Reading the debug directory of a PE file `len` bytes long, from readers
/// that can only go forward.
///
/// The caller supplies a fresh reader for each pass, which is what lets a
/// borrowed stream (an entry of an open archive) be restarted without this
/// module holding on to it. The first pass reads everything in file order and
/// is enough whenever each entry's data follows the directory — which is how
/// Roslyn and the MSVC linker lay it out. Otherwise a second pass collects the
/// data it skipped past, and that one always finishes.
#[derive(Debug)]
pub struct DebugDirectoryScan {
    len: u64,
    /// Records whose data is still to be read, after a first pass fell short.
    pending: Option<Vec<Record>>,
}

/// One debug-directory entry: its type, time stamp, minor version, data size
/// and data file offset.
type Record = (u32, u32, u16, usize, u64);

impl DebugDirectoryScan {
    pub fn new(len: u64) -> Self {
        Self {
            len: len.min(MAX_SCAN_BYTES),
            pending: None,
        }
    }

    pub fn pass(&mut self, reader: &mut dyn Read) -> io::Result<Pass> {
        let mut file = Forward::new(reader, self.len);
        // A second pass reads nothing but the pending records, in order from
        // the start, so it can never fall behind; it always finishes.
        let first = self.pending.is_none();
        let records = match self.pending.take() {
            Some(records) => records,
            None => match locate(&mut file)? {
                Located::Records(records) => records,
                Located::Done(directory) => return Ok(Pass::Done(directory)),
            },
        };
        match read_records(&mut file, &records)? {
            Some(directory) => Ok(Pass::Done(Some(directory))),
            None if first && file.behind => {
                self.pending = Some(records);
                Ok(Pass::Again)
            }
            None => Ok(Pass::Done(None)),
        }
    }
}

/// Read the debug directory from a seekable file, restarting from the start
/// as needed. For tests and for callers that have one.
pub fn read_debug_directory(
    file: &mut (impl Read + io::Seek),
    len: u64,
) -> io::Result<Option<DebugDirectory>> {
    let mut scan = DebugDirectoryScan::new(len);
    loop {
        file.seek(io::SeekFrom::Start(0))?;
        match scan.pass(file)? {
            Pass::Done(directory) => return Ok(directory),
            Pass::Again => continue,
        }
    }
}

enum Located {
    Records(Vec<Record>),
    Done(Option<DebugDirectory>),
}

fn locate(file: &mut Forward<'_>) -> io::Result<Located> {
    macro_rules! read {
        ($at:expr, $n:expr) => {
            match file.read_at($at, $n)? {
                Some(bytes) => bytes,
                None => return Ok(Located::Done(None)),
            }
        };
    }
    macro_rules! get {
        ($opt:expr) => {
            match $opt {
                Some(v) => v,
                None => return Ok(Located::Done(None)),
            }
        };
    }

    // DOS header: `MZ`, and the PE header's offset at 0x3C.
    let dos = read!(0, 64);
    if &dos[..2] != b"MZ" {
        return Ok(Located::Done(None));
    }
    let pe_at = u32_at(&dos, 0x3C) as u64;

    // PE signature and COFF header.
    let coff = read!(pe_at, 24);
    if &coff[..4] != b"PE\0\0" {
        return Ok(Located::Done(None));
    }
    let sections = u16_at(&coff, 6) as usize;
    let optional_size = u16_at(&coff, 20) as usize;
    if sections > MAX_SECTIONS || optional_size > 1024 {
        return Ok(Located::Done(None));
    }

    // Optional header: the data directory table, whose seventh entry is the
    // debug directory. Its position depends on PE32 vs PE32+.
    let optional_at = pe_at + 24;
    let optional = read!(optional_at, optional_size);
    let (count_at, directories_at) = match u16_at_checked(&optional, 0) {
        Some(0x10b) => (92, 96),
        Some(0x20b) => (108, 112),
        _ => return Ok(Located::Done(None)),
    };
    let directory_count = get!(u32_at_checked(&optional, count_at));
    if directory_count <= 6 {
        return Ok(Located::Done(Some(DebugDirectory::default())));
    }
    let debug_rva = get!(u32_at_checked(&optional, directories_at + 6 * 8));
    let debug_size = get!(u32_at_checked(&optional, directories_at + 6 * 8 + 4)) as usize;
    if debug_rva == 0 || debug_size == 0 {
        return Ok(Located::Done(Some(DebugDirectory::default())));
    }

    // Section table, to turn the directory's RVA into a file offset.
    let table = read!(optional_at + optional_size as u64, sections * 40);
    let debug_at = get!((0..sections).find_map(|i| {
        let s = &table[i * 40..i * 40 + 40];
        let virtual_address = u32_at(s, 12);
        let raw_size = u32_at(s, 16);
        let raw_pointer = u32_at(s, 20);
        // Only the part of a section backed by file bytes can hold it.
        let delta = debug_rva.checked_sub(virtual_address)?;
        (delta < raw_size).then(|| raw_pointer as u64 + delta as u64)
    }));

    let entries = (debug_size / DEBUG_ENTRY_SIZE).min(MAX_DEBUG_ENTRIES);
    let directory = read!(debug_at, entries * DEBUG_ENTRY_SIZE);
    let mut records: Vec<Record> = (0..entries)
        .map(|i| {
            let e = &directory[i * DEBUG_ENTRY_SIZE..(i + 1) * DEBUG_ENTRY_SIZE];
            (
                u32_at(e, 12),
                u32_at(e, 4),
                u16_at(e, 10),
                u32_at(e, 16) as usize,
                u32_at(e, 24) as u64,
            )
        })
        .filter(|&(kind, ..)| kind == DEBUG_TYPE_CODEVIEW || kind == DEBUG_TYPE_PDB_CHECKSUM)
        .collect();
    // In file order, so one forward pass can read them all.
    records.sort_by_key(|r| r.4);
    Ok(Located::Records(records))
}

/// Read the data of each record, in file order. `None` when a read fails —
/// with `file.behind` set when that was only because this pass had already
/// gone past it.
fn read_records(file: &mut Forward<'_>, records: &[Record]) -> io::Result<Option<DebugDirectory>> {
    macro_rules! read {
        ($at:expr, $n:expr) => {
            match file.read_at($at, $n)? {
                Some(bytes) => bytes,
                None => return Ok(None),
            }
        };
    }

    let mut out = DebugDirectory::default();
    for &(kind, stamp, minor, size, pointer) in records {
        match kind {
            // `RSDS`, GUID (16), age (4), then the PDB path.
            DEBUG_TYPE_CODEVIEW if size >= 24 => {
                let data = read!(pointer, 24);
                if &data[..4] != b"RSDS" {
                    continue;
                }
                let mut guid = [0u8; 16];
                guid.copy_from_slice(&data[4..20]);
                out.codeview.push(CodeView {
                    guid,
                    age: u32_at(&data, 20),
                    stamp,
                    portable: minor == PORTABLE_CODEVIEW_MINOR,
                });
            }
            // The algorithm name, NUL-terminated, then the checksum.
            DEBUG_TYPE_PDB_CHECKSUM if size > 1 && size <= 256 => {
                let data = read!(pointer, size);
                let Some(nul) = data.iter().position(|&b| b == 0) else {
                    continue;
                };
                let Ok(algorithm) = std::str::from_utf8(&data[..nul]) else {
                    continue;
                };
                out.checksums.push(PdbChecksum {
                    algorithm: algorithm.to_string(),
                    checksum: data[nul + 1..].to_vec(),
                });
            }
            _ => {}
        }
    }
    Ok(Some(out))
}

/// A forward-only reader addressed by file offset: a read ahead of the
/// current position skips to it; one behind it fails and sets `behind`.
struct Forward<'a> {
    reader: &'a mut dyn Read,
    pos: u64,
    len: u64,
    behind: bool,
}

impl<'a> Forward<'a> {
    fn new(reader: &'a mut dyn Read, len: u64) -> Self {
        Self {
            reader,
            pos: 0,
            len,
            behind: false,
        }
    }

    /// `n` bytes at `at`, or `None` when that lies outside the file (or the
    /// scan limit), behind this pass, or past where the stream really ends.
    fn read_at(&mut self, at: u64, n: usize) -> io::Result<Option<Vec<u8>>> {
        if at.checked_add(n as u64).is_none_or(|end| end > self.len) {
            return Ok(None);
        }
        if self.pos == u64::MAX {
            return Ok(None);
        }
        if at < self.pos {
            self.behind = true;
            return Ok(None);
        }
        let skip = at - self.pos;
        let skipped = io::copy(&mut (&mut *self.reader).take(skip), &mut io::sink())?;
        self.pos += skipped;
        if skipped != skip {
            return Ok(None);
        }
        let mut buf = vec![0u8; n];
        if self.reader.read_exact(&mut buf).is_err() {
            // Shorter than declared; nothing further on can be read either.
            self.pos = u64::MAX;
            return Ok(None);
        }
        self.pos += n as u64;
        Ok(Some(buf))
    }
}

fn u16_at(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

fn u16_at_checked(buf: &[u8], at: usize) -> Option<u16> {
    let b = buf.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at_checked(buf: &[u8], at: usize) -> Option<u32> {
    let b = buf.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Build a minimal PE32 image whose debug directory holds a Portable CodeView
/// entry and, optionally, a `PdbChecksum` entry — enough for the reader above
/// and nothing more. For tests, here and in [`crate::symbols`].
#[cfg(test)]
pub(crate) fn test_image(guid: &[u8; 16], stamp: u32, checksum: Option<(&str, &[u8])>) -> Vec<u8> {
    const SECTION_RVA: u32 = 0x2000;
    const SECTION_RAW: u32 = 0x200;
    let mut image = vec![0u8; SECTION_RAW as usize];
    image[..2].copy_from_slice(b"MZ");
    let pe_at = 0x80usize;
    image[0x3C..0x40].copy_from_slice(&(pe_at as u32).to_le_bytes());
    image[pe_at..pe_at + 4].copy_from_slice(b"PE\0\0");
    let coff = pe_at + 4;
    image[coff..coff + 2].copy_from_slice(&0x14Cu16.to_le_bytes()); // i386
    image[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // sections
    image[coff + 16..coff + 18].copy_from_slice(&224u16.to_le_bytes()); // optional size
    let optional = coff + 20;
    image[optional..optional + 2].copy_from_slice(&0x10Bu16.to_le_bytes());
    image[optional + 92..optional + 96].copy_from_slice(&16u32.to_le_bytes());

    // Section data: the debug directory, then each entry's data.
    let entries = if checksum.is_some() { 2 } else { 1 };
    let mut data = vec![0u8; entries * DEBUG_ENTRY_SIZE];
    let entry =
        |index: usize, kind: u32, major: u16, minor: u16, payload: &[u8], data: &mut Vec<u8>| {
            let at = data.len();
            data.extend_from_slice(payload);
            let e = &mut data[index * DEBUG_ENTRY_SIZE..(index + 1) * DEBUG_ENTRY_SIZE];
            e[4..8].copy_from_slice(
                &if kind == DEBUG_TYPE_CODEVIEW {
                    stamp
                } else {
                    0
                }
                .to_le_bytes(),
            );
            e[8..10].copy_from_slice(&major.to_le_bytes());
            e[10..12].copy_from_slice(&minor.to_le_bytes());
            e[12..16].copy_from_slice(&kind.to_le_bytes());
            e[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            e[20..24].copy_from_slice(&(SECTION_RVA + at as u32).to_le_bytes());
            e[24..28].copy_from_slice(&(SECTION_RAW + at as u32).to_le_bytes());
        };
    let mut codeview = b"RSDS".to_vec();
    codeview.extend_from_slice(guid);
    codeview.extend_from_slice(&1u32.to_le_bytes());
    codeview.extend_from_slice(b"Lib.pdb\0");
    entry(
        0,
        DEBUG_TYPE_CODEVIEW,
        0x0100,
        PORTABLE_CODEVIEW_MINOR,
        &codeview,
        &mut data,
    );
    if let Some((algorithm, hash)) = checksum {
        let mut payload = algorithm.as_bytes().to_vec();
        payload.push(0);
        payload.extend_from_slice(hash);
        entry(1, DEBUG_TYPE_PDB_CHECKSUM, 1, 0, &payload, &mut data);
    }

    // Debug data directory (index 6) and the one section holding it.
    let directory = optional + 96 + 6 * 8;
    image[directory..directory + 4].copy_from_slice(&SECTION_RVA.to_le_bytes());
    image[directory + 4..directory + 8]
        .copy_from_slice(&((entries * DEBUG_ENTRY_SIZE) as u32).to_le_bytes());
    let section = optional + 224;
    image[section..section + 5].copy_from_slice(b".text");
    image[section + 8..section + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
    image[section + 12..section + 16].copy_from_slice(&SECTION_RVA.to_le_bytes());
    image[section + 16..section + 20].copy_from_slice(&(data.len() as u32).to_le_bytes());
    image[section + 20..section + 24].copy_from_slice(&SECTION_RAW.to_le_bytes());
    image.extend_from_slice(&data);
    image
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(image: &[u8]) -> Option<DebugDirectory> {
        read_debug_directory(&mut std::io::Cursor::new(image), image.len() as u64).unwrap()
    }

    #[test]
    fn reads_codeview_and_checksum_entries() {
        let guid = [0x11u8; 16];
        let image = test_image(&guid, 0xAABB_CCDD, Some(("SHA256", &[7u8; 32])));
        let directory = read(&image).expect("a PE image");
        assert_eq!(
            directory.codeview,
            vec![CodeView {
                guid,
                age: 1,
                stamp: 0xAABB_CCDD,
                portable: true,
            }]
        );
        assert_eq!(directory.checksums.len(), 1);
        assert_eq!(directory.checksums[0].algorithm, "SHA256");
        assert_eq!(directory.checksums[0].checksum, vec![7u8; 32]);
    }

    #[test]
    fn non_images_are_none() {
        assert!(read(b"").is_none());
        assert!(read(b"not a pe file at all, no").is_none());
        assert!(read(&[b'M', b'Z', 0, 0]).is_none());
    }

    /// Every offset comes from the file. Truncating or corrupting any byte of
    /// the headers must produce `None` or a different answer, never a panic.
    #[test]
    fn hostile_images_do_not_panic() {
        let image = test_image(&[3u8; 16], 5, Some(("SHA256", &[1u8; 32])));
        for len in 0..image.len() {
            let _ = read(&image[..len]);
        }
        for at in 0..0x200.min(image.len()) {
            for value in [0x00, 0xFF, 0x7F] {
                let mut corrupt = image.clone();
                corrupt[at] = value;
                let _ = read(&corrupt);
            }
        }
    }

    /// Records behind the current position are reached by reopening.
    #[test]
    fn a_second_pass_reads_records_behind_the_first() {
        let mut image = test_image(&[9u8; 16], 1, None);
        // Point the CodeView entry at a copy of its data in the unused gap
        // between the section table and the section, which the reader has
        // passed by the time it reads the directory.
        let directory_at = 0x200;
        let pointer = u32_at(&image, directory_at + 24) as usize;
        let payload: Vec<u8> = image[pointer..pointer + 24].to_vec();
        image[0x1C0..0x1D8].copy_from_slice(&payload);
        image[directory_at + 24..directory_at + 28].copy_from_slice(&0x1C0u32.to_le_bytes());

        let mut scan = DebugDirectoryScan::new(image.len() as u64);
        assert!(matches!(
            scan.pass(&mut image.as_slice()).unwrap(),
            Pass::Again
        ));
        let Pass::Done(Some(directory)) = scan.pass(&mut image.as_slice()).unwrap() else {
            panic!("the second pass must finish");
        };
        assert_eq!(directory.codeview[0].guid, [9u8; 16]);
    }

    /// A stream shorter than its declared length ends the scan; it must not
    /// be mistaken for a record behind the position and loop.
    #[test]
    fn a_short_stream_finishes() {
        let image = test_image(&[9u8; 16], 1, Some(("SHA256", &[0u8; 32])));
        let short = &image[..image.len() - 10];
        let mut scan = DebugDirectoryScan::new(image.len() as u64);
        let mut passes = 0;
        loop {
            passes += 1;
            assert!(passes <= 2, "the scan did not finish");
            if let Pass::Done(_) = scan.pass(&mut &short[..]).unwrap() {
                break;
            }
        }
    }
}
