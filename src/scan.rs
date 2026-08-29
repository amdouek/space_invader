use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jwalk::WalkDirGeneric;

use crate::units::{commas, human};

pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
}

pub struct ScanResult {
    pub root: PathBuf,
    pub files: Vec<FileEntry>,
    /// Cumulative size of every directory (including all subdirectories).
    pub dir_sizes: HashMap<PathBuf, u64>,
    pub dir_count: u64,
    pub error_count: u64,
    pub elapsed: Duration,
    pub total_size: u64,
}

/// Walk each file's ancestors to aggregate cumulative directory sizes.
pub fn aggregate_dirs(root: &Path, files: &[FileEntry]) -> HashMap<PathBuf, u64> {
    let mut dir_sizes: HashMap<PathBuf, u64> = HashMap::new();
    dir_sizes.insert(root.to_path_buf(), 0);
    for f in files {
        let mut p = f.path.parent();
        while let Some(dir) = p {
            *dir_sizes.entry(dir.to_path_buf()).or_insert(0) += f.size;
            if dir == root {
                break;
            }
            p = dir.parent();
        }
    }
    dir_sizes
}

/// Traverse `root` in parallel, collecting file sizes and directory totals.
pub fn scan(root: &Path, threads: Option<usize>) -> ScanResult {
    let start = Instant::now();

    let files_seen = Arc::new(AtomicU64::new(0));
    let bytes_seen = Arc::new(AtomicU64::new(0));
    let meta_errors = Arc::new(AtomicU64::new(0));

    // Progress ticker on stderr.
    let stop = Arc::new(AtomicBool::new(false));
    let ticker = {
        let files_seen = files_seen.clone();
        let bytes_seen = bytes_seen.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                eprint!(
                    "\r  scanning... {} files, {}          ",
                    commas(files_seen.load(Ordering::Relaxed)),
                    human(bytes_seen.load(Ordering::Relaxed))
                );
                let _ = std::io::stderr().flush();
                std::thread::sleep(Duration::from_millis(250));
            }
        })
    };

    let mut walk = WalkDirGeneric::<((), Option<u64>)>::new(root)
        .skip_hidden(false)
        .follow_links(false);
    if let Some(n) = threads {
        walk = walk.parallelism(jwalk::Parallelism::RayonNewPool(n));
    }
    let walk = {
        let files_seen = files_seen.clone();
        let bytes_seen = bytes_seen.clone();
        let meta_errors = meta_errors.clone();
        walk.process_read_dir(move |_depth, _path, _state, children| {
            for child in children.iter_mut() {
                if let Ok(entry) = child {
                    if entry.file_type.is_file() {
                        match entry.metadata() {
                            Ok(md) => {
                                let len = md.len();
                                entry.client_state = Some(len);
                                files_seen.fetch_add(1, Ordering::Relaxed);
                                bytes_seen.fetch_add(len, Ordering::Relaxed);
                            }
                            Err(_) => {
                                meta_errors.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            }
        })
    };

    let mut files: Vec<FileEntry> = Vec::new();
    let mut dir_count: u64 = 0;
    let mut walk_errors: u64 = 0;

    for entry in walk {
        match entry {
            Ok(e) => {
                if e.file_type.is_dir() {
                    dir_count += 1;
                } else if let Some(size) = e.client_state {
                    files.push(FileEntry {
                        path: e.path(),
                        size,
                    });
                }
            }
            Err(_) => walk_errors += 1,
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = ticker.join();
    eprint!("\r{}\r", " ".repeat(70));
    let _ = std::io::stderr().flush();

    let dir_sizes = aggregate_dirs(root, &files);
    let total_size = dir_sizes.get(root).copied().unwrap_or(0);

    ScanResult {
        root: root.to_path_buf(),
        files,
        dir_sizes,
        dir_count,
        error_count: walk_errors + meta_errors.load(Ordering::Relaxed),
        elapsed: start.elapsed(),
        total_size,
    }
}
