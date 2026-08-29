use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;
use xxhash_rust::xxh3::{xxh3_128, Xxh3};

use crate::scan::FileEntry;
use crate::units::commas;

const SAMPLE_CHUNK: u64 = 64 * 1024;
/// Files at or below this size are fully read and hashed in the sample stage.
const SMALL_FULL: u64 = 256 * 1024;
/// Floor for "same size + same name" likely-duplicate matching.
pub const NAME_FLOOR: u64 = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SampleOutcome {
    /// The sample covered the whole file, so this is a full-content hash.
    Full(u128),
    /// Hash of first/middle/last chunks only.
    Sample(u128),
    Error,
}

pub struct DupeGroup {
    /// Size of each file in the group.
    pub size: u64,
    /// Bytes reclaimable if only one copy were kept: size * (count - 1).
    pub wasted: u64,
    /// Hash hex (absolute), or shared filename (likely-by-name), or sample hash hex.
    pub key: String,
    pub files: Vec<String>,
}

pub struct DupeReport {
    /// Full-content hash confirmed identical.
    pub absolute: Vec<DupeGroup>,
    /// Content samples match but the files exceeded the full-hash cap.
    pub likely_sample: Vec<DupeGroup>,
    /// Same size + same filename; content not hashed.
    pub likely_name: Vec<DupeGroup>,
    pub candidates_hashed: u64,
}

impl DupeReport {
    pub fn wasted_absolute(&self) -> u64 {
        self.absolute.iter().map(|g| g.wasted).sum()
    }
    pub fn wasted_likely(&self) -> u64 {
        self.likely_sample.iter().map(|g| g.wasted).sum::<u64>()
            + self.likely_name.iter().map(|g| g.wasted).sum::<u64>()
    }
}

fn read_exact_at(f: &mut File, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(buf)
}

fn sample_hash(path: &Path, size: u64) -> std::io::Result<SampleOutcome> {
    let mut f = File::open(path)?;
    if size <= SMALL_FULL {
        let mut buf = Vec::with_capacity(size as usize);
        f.read_to_end(&mut buf)?;
        Ok(SampleOutcome::Full(xxh3_128(&buf)))
    } else {
        let mut hasher = Xxh3::new();
        let mut buf = vec![0u8; SAMPLE_CHUNK as usize];
        read_exact_at(&mut f, 0, &mut buf)?;
        hasher.update(&buf);
        read_exact_at(&mut f, size / 2 - SAMPLE_CHUNK / 2, &mut buf)?;
        hasher.update(&buf);
        read_exact_at(&mut f, size - SAMPLE_CHUNK, &mut buf)?;
        hasher.update(&buf);
        Ok(SampleOutcome::Sample(hasher.digest128()))
    }
}

fn full_hash(path: &Path) -> std::io::Result<u128> {
    let mut f = File::open(path)?;
    let mut hasher = Xxh3::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.digest128())
}

/// Tiered duplicate detection.
///
/// 1. Group files by exact size; only same-size files can be duplicates.
/// 2. Sample-hash candidates >= `min_size` (small candidates get a full read).
/// 3. Full-hash sample-matched files up to `max_full` -> "absolute" duplicates.
///    Sample-matched files above the cap -> "likely (samples match)".
/// 4. Unhashed files sharing size + filename -> "likely (same size & name)".
pub fn find_dupes(files: &[FileEntry], min_size: u64, max_full: u64) -> DupeReport {
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if f.size > 0 {
            by_size.entry(f.size).or_default().push(i);
        }
    }

    let candidates: Vec<usize> = by_size
        .iter()
        .filter(|(sz, v)| **sz >= min_size && v.len() >= 2)
        .flat_map(|(_, v)| v.iter().copied())
        .collect();

    // Stage 1: Parallel sample hashing.
    let done = AtomicU64::new(0);
    let n_cand = candidates.len() as u64;
    let sample_results: Vec<(usize, SampleOutcome)> = candidates
        .par_iter()
        .map(|&i| {
            let f = &files[i];
            let h = sample_hash(&f.path, f.size).unwrap_or(SampleOutcome::Error);
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d % 512 == 0 || d == n_cand {
                eprint!("\r  sampling {} / {} candidate files    ", commas(d), commas(n_cand));
                let _ = std::io::stderr().flush();
            }
            (i, h)
        })
        .collect();
    if n_cand > 0 {
        eprint!("\r{}\r", " ".repeat(60));
    }

    let mut errored: HashSet<usize> = HashSet::new();
    let mut sample_groups: HashMap<(u64, SampleOutcome), Vec<usize>> = HashMap::new();
    for (i, h) in sample_results {
        match h {
            SampleOutcome::Error => {
                errored.insert(i);
            }
            _ => sample_groups.entry((files[i].size, h)).or_default().push(i),
        }
    }

    // Stage 2: Split sample-matched groups into full-hash work/already-full/over-cap.
    let mut absolute_map: HashMap<(u64, u128), Vec<usize>> = HashMap::new();
    let mut over_cap_groups: Vec<Vec<usize>> = Vec::new();
    let mut to_full_hash: Vec<usize> = Vec::new();

    for ((size, outcome), members) in &sample_groups {
        if members.len() < 2 {
            continue;
        }
        match outcome {
            SampleOutcome::Full(h) => {
                absolute_map.entry((*size, *h)).or_default().extend(members);
            }
            SampleOutcome::Sample(_) => {
                if *size <= max_full {
                    to_full_hash.extend(members);
                } else {
                    over_cap_groups.push(members.clone());
                }
            }
            SampleOutcome::Error => unreachable!(),
        }
    }

    let done = AtomicU64::new(0);
    let n_full = to_full_hash.len() as u64;
    let full_results: Vec<(usize, Option<u128>)> = to_full_hash
        .par_iter()
        .map(|&i| {
            let h = full_hash(&files[i].path).ok();
            let d = done.fetch_add(1, Ordering::Relaxed) + 1;
            if d % 16 == 0 || d == n_full {
                eprint!("\r  full-hashing {} / {} files    ", commas(d), commas(n_full));
                let _ = std::io::stderr().flush();
            }
            (i, h)
        })
        .collect();
    if n_full > 0 {
        eprint!("\r{}\r", " ".repeat(60));
    }
    let _ = std::io::stderr().flush();

    for (i, h) in full_results {
        match h {
            Some(h) => absolute_map.entry((files[i].size, h)).or_default().push(i),
            None => {
                errored.insert(i);
            }
        }
    }

    let mut in_group: HashSet<usize> = HashSet::new();
    let mut absolute: Vec<DupeGroup> = Vec::new();
    for ((size, h), members) in absolute_map {
        if members.len() < 2 {
            continue;
        }
        in_group.extend(&members);
        absolute.push(make_group(files, *&size, format!("{h:032x}"), &members));
    }

    let mut likely_sample: Vec<DupeGroup> = Vec::new();
    for members in over_cap_groups {
        in_group.extend(&members);
        let size = files[members[0]].size;
        likely_sample.push(make_group(files, size, "content samples match".into(), &members));
    }

    // Stage 3: Same size + same filename among files hashing never resolved.
    let hashed: HashSet<usize> = candidates
        .iter()
        .copied()
        .filter(|i| !errored.contains(i))
        .collect();
    let mut by_name: HashMap<(u64, String), Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if f.size < NAME_FLOOR || in_group.contains(&i) || hashed.contains(&i) {
            continue;
        }
        let name = match f.path.file_name() {
            Some(n) => n.to_string_lossy().to_lowercase(),
            None => continue,
        };
        by_name.entry((f.size, name)).or_default().push(i);
    }
    let mut likely_name: Vec<DupeGroup> = Vec::new();
    for ((size, name), members) in by_name {
        if members.len() < 2 {
            continue;
        }
        likely_name.push(make_group(files, size, name, &members));
    }

    absolute.sort_by(|a, b| b.wasted.cmp(&a.wasted));
    likely_sample.sort_by(|a, b| b.wasted.cmp(&a.wasted));
    likely_name.sort_by(|a, b| b.wasted.cmp(&a.wasted));

    DupeReport {
        absolute,
        likely_sample,
        likely_name,
        candidates_hashed: n_cand,
    }
}

fn make_group(files: &[FileEntry], size: u64, key: String, members: &[usize]) -> DupeGroup {
    let mut paths: Vec<String> = members
        .iter()
        .map(|&i| files[i].path.display().to_string())
        .collect();
    paths.sort();
    DupeGroup {
        size,
        wasted: size * (members.len() as u64 - 1),
        key,
        files: paths,
    }
}
