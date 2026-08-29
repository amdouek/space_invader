//! MFT fast path: read the NTFS Master File Table directly from the raw volume
//! instead of walking directories. Requires an elevated process and a local
//! NTFS volume; callers fall back to the standard walker when `scan` errors.
//! Read-only like everything else: the volume handle is opened for read access.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write as _};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ntfs::structured_values::{NtfsFileName, NtfsFileNamespace};
use ntfs::{Ntfs, NtfsAttributeType, NtfsFileFlags};

use crate::scan::{aggregate_dirs, FileEntry, ScanResult};
use crate::units::commas;

const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const FILE_SHARE_DELETE: u32 = 0x4;
/// NTFS file record number of the volume root directory.
const ROOT_FRN: u64 = 5;
const CHUNK: usize = 4 * 1024 * 1024;
const SECTOR: u64 = 4096;

/// Raw volume reads must happen at sector-aligned offsets in sector-multiple
/// lengths. This reader satisfies that while caching a large aligned window,
/// so the mostly-sequential record reads of an MFT sweep hit the cache.
struct ChunkReader {
    inner: File,
    buf: Vec<u8>,
    buf_start: u64,
    buf_len: usize,
    pos: u64,
}

impl ChunkReader {
    fn new(inner: File) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            buf_start: 0,
            buf_len: 0,
            pos: 0,
        }
    }

    fn fill(&mut self, pos: u64) -> io::Result<()> {
        let start = pos / SECTOR * SECTOR;
        let mut len = CHUNK;
        loop {
            self.inner.seek(SeekFrom::Start(start))?;
            self.buf.resize(len, 0);
            match read_until_full(&mut self.inner, &mut self.buf) {
                Ok(n) if n > 0 => {
                    self.buf_start = start;
                    self.buf_len = n;
                    return Ok(());
                }
                Ok(_) => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "end of volume"))
                }
                // Windows fails the whole read if it extends past the end of
                // the volume; retry with smaller sector-multiple lengths.
                Err(e) => {
                    if len as u64 > SECTOR {
                        len /= 2;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }
}

fn read_until_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => {
                if filled > 0 {
                    break;
                }
                return Err(e);
            }
        }
    }
    Ok(filled)
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut written = 0;
        while written < out.len() {
            let in_window =
                self.pos >= self.buf_start && self.pos < self.buf_start + self.buf_len as u64;
            if !in_window {
                match self.fill(self.pos) {
                    Ok(()) => {}
                    Err(_) if written > 0 => break,
                    Err(e) => return Err(e),
                }
            }
            let off = (self.pos - self.buf_start) as usize;
            let n = (out.len() - written).min(self.buf_len - off);
            if n == 0 {
                break;
            }
            out[written..written + n].copy_from_slice(&self.buf[off..off + n]);
            written += n;
            self.pos += n as u64;
        }
        Ok(written)
    }
}

impl Seek for ChunkReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.pos = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => self
                .pos
                .checked_add_signed(d)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad seek"))?,
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "SeekFrom::End unsupported on raw volume",
                ))
            }
        };
        Ok(self.pos)
    }
}

struct Rec {
    name: String,
    parent: u64,
    size: u64,
    is_dir: bool,
}

/// Scan by sweeping the volume's MFT. Returns the same `ScanResult` the
/// directory walker produces, restricted to files under `root`.
pub fn scan(root: &Path) -> Result<ScanResult, String> {
    let start = Instant::now();

    let s = root.to_string_lossy();
    let b = s.as_bytes();
    if !(b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') {
        return Err("requires a local drive path like C:\\".into());
    }
    let drive = b[0].to_ascii_uppercase() as char;
    let drive_root = PathBuf::from(format!("{drive}:\\"));

    let vol = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(format!("\\\\.\\{drive}:"))
        .map_err(|e| format!("cannot open volume \\\\.\\{drive}: ({e}); administrator rights required"))?;

    let mut fs = ChunkReader::new(vol);
    let ntfs = Ntfs::new(&mut fs).map_err(|e| format!("not readable as NTFS: {e}"))?;
    let record_size = ntfs.file_record_size() as u64;

    // Record count = size of the MFT's own unnamed $DATA / record size.
    let mft = ntfs
        .file(&mut fs, 0)
        .map_err(|e| format!("cannot read $MFT record: {e}"))?;
    let mft_data = mft
        .data(&mut fs, "")
        .ok_or_else(|| "$MFT has no data attribute".to_string())?
        .and_then(|item| item.to_attribute().map(|a| a.value_length()))
        .map_err(|e| format!("cannot size $MFT: {e}"))?;
    let record_count = mft_data / record_size;

    let mut recs: HashMap<u64, Rec> = HashMap::with_capacity(record_count as usize);
    for n in 0..record_count {
        if n % 131_072 == 0 {
            eprint!(
                "\r  reading MFT... {} / {} records    ",
                commas(n),
                commas(record_count)
            );
            let _ = io::stderr().flush();
        }
        let file = match ntfs.file(&mut fs, n) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if n == ROOT_FRN || !file.flags().contains(NtfsFileFlags::IN_USE) {
            continue;
        }
        let is_dir = file.is_directory();

        let mut best: Option<(bool, String, u64)> = None; // (is_dos_only, name, parent)
        let mut size: Option<u64> = None;
        let mut iter = file.attributes();
        while let Some(item) = iter.next(&mut fs) {
            let Ok(item) = item else { continue };
            let Ok(attr) = item.to_attribute() else { continue };
            match attr.ty() {
                Ok(NtfsAttributeType::FileName) => {
                    if let Ok(fname) = attr.structured_value::<_, NtfsFileName>(&mut fs) {
                        let dos_only = fname.namespace() == NtfsFileNamespace::Dos;
                        let better = match &best {
                            None => true,
                            Some((best_dos_only, _, _)) => *best_dos_only && !dos_only,
                        };
                        if better {
                            best = Some((
                                dos_only,
                                fname.name().to_string_lossy(),
                                fname.parent_directory_reference().file_record_number(),
                            ));
                        }
                    }
                }
                Ok(NtfsAttributeType::Data) => {
                    if size.is_none() {
                        if let Ok(name) = attr.name() {
                            if name.u16_iter().next().is_none() {
                                size = Some(attr.value_length());
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Records without a $FILE_NAME are extension records or otherwise
        // unaddressable; NTFS metafiles ($MFT, $Bitmap, $Extend, ...) live
        // under the root with '$'-prefixed names and are skipped to keep
        // totals comparable with the directory walker.
        let Some((_, name, parent)) = best else { continue };
        if parent == ROOT_FRN && name.starts_with('$') {
            continue;
        }
        recs.insert(
            n,
            Rec {
                name,
                parent,
                size: size.unwrap_or(0),
                is_dir,
            },
        );
    }
    eprint!("\r{}\r", " ".repeat(60));
    let _ = io::stderr().flush();

    // Resolve directory paths (memoized); files under unresolvable chains
    // (orphaned or skipped ancestors) are dropped.
    let mut dir_paths: HashMap<u64, Option<PathBuf>> = HashMap::new();
    dir_paths.insert(ROOT_FRN, Some(drive_root.clone()));

    let mut files: Vec<FileEntry> = Vec::new();
    let mut dir_count: u64 = 0;
    let scanning_whole_volume = root == drive_root;

    for (frn, rec) in &recs {
        if rec.is_dir {
            if let Some(p) = resolve_dir(*frn, &recs, &mut dir_paths) {
                if scanning_whole_volume || p.starts_with(root) {
                    dir_count += 1;
                }
            }
        } else if let Some(parent_path) = resolve_dir(rec.parent, &recs, &mut dir_paths) {
            let path = parent_path.join(&rec.name);
            if scanning_whole_volume || path.starts_with(root) {
                files.push(FileEntry {
                    path,
                    size: rec.size,
                });
            }
        }
    }
    if scanning_whole_volume {
        dir_count += 1; // the root itself, as the walker counts it
    }

    let dir_sizes = aggregate_dirs(root, &files);
    let total_size = dir_sizes.get(root).copied().unwrap_or(0);

    Ok(ScanResult {
        root: root.to_path_buf(),
        files,
        dir_sizes,
        dir_count,
        error_count: 0,
        elapsed: start.elapsed(),
        total_size,
    })
}

fn resolve_dir(
    frn: u64,
    recs: &HashMap<u64, Rec>,
    memo: &mut HashMap<u64, Option<PathBuf>>,
) -> Option<PathBuf> {
    // Iterative ancestor collection so deep trees can't overflow the stack;
    // the visited set breaks cycles in corrupt parent chains.
    let mut chain: Vec<u64> = Vec::new();
    let mut cur = frn;
    let mut base: Option<PathBuf> = None;
    for _ in 0..512 {
        if let Some(known) = memo.get(&cur) {
            base = known.clone();
            break;
        }
        let Some(rec) = recs.get(&cur) else { break };
        if !rec.is_dir || chain.contains(&cur) {
            break;
        }
        chain.push(cur);
        cur = rec.parent;
    }
    let Some(mut path) = base else {
        for f in chain {
            memo.insert(f, None);
        }
        return None;
    };
    for f in chain.into_iter().rev() {
        path = path.join(&recs[&f].name);
        memo.insert(f, Some(path.clone()));
    }
    Some(path)
}
