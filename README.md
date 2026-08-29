# 👾 space_invader

A super-fast, strictly read-only disk space scanner. 

Using WinDirStat (or worse, Explorer) to figure out what is taking up space on your drive is an enormous time sink. ***`space_invader`*** is a Rust-based solution to this problem - point it at any location on your device, and it tells you what the culprit files and folders are, and whether there are any absolute or likely duplicate files you should probably take care of.


## Usage

```
space_invader <PATH> [options]
```

Examples:

```
space_invader C:\Users\me
space_invader D:\ --open
space_invader C:\ --top 25 --report D:\scans\c_drive.html
space_invader C:\Projects --no-dupes
```

Output is a CLI summary and an interactive HTML report
(`space_invader_report.html` by default) with:

- An interactive **treemap** (similar to WinDirStat but less horrifically ugly). The map's tile area correlates to size on disk; click tiles to zoom into a folder, hover for details
- Ranked tables of the **largest folders and files**
- **Duplicate groups** ranked by reclaimable space

The report is a single offline HTML file.

## Options

| Flag | Default | Meaning |
|---|---|---|
| `--top N` | 15 | Rows per table in the CLI summary |
| `--report PATH` | `space_invader_report.html` | Where to write the HTML report |
| `--no-report` | | Skip the HTML report |
| `--no-dupes` | | Skip duplicate detection |
| `--dupe-min-size SIZE` | `1M` | Minimum file size considered for hash-based duplicate detection (`500K`, `1M`, `1G`…) |
| `--max-full-hash SIZE` | `4G` | Files larger than this are sample-hashed only and duplicates flagged only as *likely* |
| `--open` | | Open the report in your default browser when done |
| `--threads N` | auto | Worker thread count |
| `--mft` | off | **Experimental, not yet validated**: read the NTFS MFT directly (needs an elevated terminal + local NTFS; falls back to the standard scan otherwise) |

## Duplicate detection approach

This has been done in a logically tiered manner to keep `space_invader` as quick as possible.

1. **Group by size.** Only files with identical byte size can be duplicates.
2. **Sample hash.** Candidates ≥ `--dupe-min-size` get the first/middle/last
   64 KiB hashed (xxh3-128), and mismatched samples are eliminated.
3. **Full hash.** Sample-matched files up to `--max-full-hash` are fully hashed.
   Identical full hashes are declared absolute duplicates.
4. **Likely duplicates** are flagged based on the following:
   - *Samples match*: Content samples are identical, but the file exceeded the
     full-hash cap;
   - *Same size & name*: Identical byte size and filename (≥ 64 KiB) in
     different folders.

## Performance

Benchmarked 2026-08-29 on an Intel Core Ultra 7 155H (16C/22T), 32 GB RAM,
Micron 3500 NVMe SSD, Windows 11. Each figure is the mean of repeated runs after
a warm-up pass (± sample standard deviation):

| Target | Contents | Scan only (5 runs) | Scan + dupes + report (3 runs) |
|---|---|---|---|
| `C:\` | 304.5 GB, 1,294,574 files | **52.1 ± 0.8 s** | 68.9 s |
| `AppData\Local` | 99.5 GB, 463,255 files | **17.2 ± 0.3 s** | 18.4 s |

That is roughly **25,000–27,000 files/s** sustained. Scan time is driven almost
entirely by *file count*, not bytes — duplicate detection adds time only in
proportion to how many same-size candidate files must be hashed (11,614
candidates on this `C:\`, adding ~17 s).

These are warm-cache numbers: Windows caches NTFS metadata after the first
pass, so the very first scan of a drive since boot is disk-bound and slower;
repeat scans settle at the figures above.

## How it compares

| Tool | Speed | Duplicate detection | Notes |
|---|---|---|---|
| **space_invader** | ~25k files/s | Content-hashed (tiered) | Read-only guarantee; portable HTML report |
| WinDirStat | Slow | None | GUI; includes delete actions |
| WizTree | ~10× faster (reads NTFS MFT directly) | Name+size only | GUI; needs admin, local NTFS only |
| dust / dua / gdu | Comparable | None | CLI only, no report |
| czkawka / fclones | Comparable (same tiered hashing) | Excellent | Dedupe-only; built to delete what they find |

space_invader's niche: the only one doing space attribution **and** content-verified
duplicates in one pass, with a shareable offline report and no code path that can
modify your files.

## Notes

- **Explicitly read-only.** The tool opens files only to read bytes for hashing.
  It has no code path that writes to, moves, or deletes anything it scans.
- File sizes are logical. NTFS compression, sparse regions, and
  allocation overhead are not reflected, so totals can differ slightly from
  "size on disk" in Explorer.
- Junctions/symlinks are **not** followed to prevent double-counting and cycles.
- Scanning `C:\` as a regular user will hit access-denied on some system
  folders (counted as "inaccessible"). Run the tool from an elevated terminal for full
  coverage.
- Hashes are xxh3-128 (non-cryptographic but effectively collision-free for
  this purpose).

## Building

```
cargo build --release
```

The binary is in `target\release\space_invader.exe` as a single .exe with no
runtime dependencies. Built with the `stable-x86_64-pc-windows-gnu` toolchain.

### macOS (experimental — compiles, not yet run on-device)

The codebase is portable Rust and type-checks cleanly for
`aarch64-apple-darwin`; platform specifics (`--open` uses `open`, the `--mft`
flag is Windows-only and absent on Mac, error hints mention Full Disk Access)
are already handled. On a Mac:

```
cargo build --release   # binary lands in target/release/space_invader
```

Things to verify on-device before trusting results (see Roadmap): TCC
permissions, firmlink handling when scanning `/`, and APFS clone semantics.

## Roadmap

- macOS on-device validation (the port is implemented and compiles for
  `aarch64-apple-darwin`; needs a real Mac to verify):
  - **TCC permissions**: without Full Disk Access for the terminal, macOS
    silently hides `~/Documents`, `~/Desktop`, `~/Downloads`, Mail, etc. —
    confirm the inaccessible-items hint appears and counts look sane
  - **Firmlinks**: scanning `/` traverses the stitched System/Data volume
    layout — check for double-counting and whether `/System/Volumes/Data`
    and `/Volumes` mounts need explicit handling (e.g. a stay-on-filesystem
    option)
  - **APFS clones**: cloned files report full logical size each, so totals
    can exceed real disk usage — decide whether to note or detect this
  - **Case-insensitivity**: APFS is case-insensitive by default — confirm
    likely-duplicate name matching behaves (it already lowercases)
  - Cosmetics: terminal colors/emoji in Terminal.app, report rendering in
    Safari
- Validate and benchmark the `--mft` fast path (WizTree-style direct MFT reading,
  expected ~10× faster full-drive scans). The code is implemented behind the
  experimental `--mft` flag and the unelevated fallback is verified, but the
  elevated path itself has not yet been run — needs an admin-elevated terminal.
  Known semantic differences from the walker to check during validation:
  hard-linked files count once (the walker counts each link path), and NTFS
  metafiles are excluded.
