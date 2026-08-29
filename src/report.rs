use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::dupes::{DupeGroup, DupeReport};
use crate::scan::ScanResult;

const MAX_TOP: usize = 500;
const MAX_GROUPS: usize = 400;
const MAX_FILES_PER_GROUP: usize = 100;
const MAX_TREE_NODES: usize = 6000;
const MAX_CHILDREN: usize = 60;
const MAX_DEPTH: usize = 10;

#[derive(Serialize)]
struct Node {
    n: String,
    s: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    c: Vec<Node>,
}

#[derive(Serialize)]
struct PathSize {
    p: String,
    s: u64,
}

#[derive(Serialize)]
struct JsonGroup {
    sz: u64,
    w: u64,
    k: String,
    f: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

#[derive(Serialize)]
struct Meta<'a> {
    root: String,
    #[serde(rename = "totalSize")]
    total_size: u64,
    #[serde(rename = "fileCount")]
    file_count: u64,
    #[serde(rename = "dirCount")]
    dir_count: u64,
    #[serde(rename = "errorCount")]
    error_count: u64,
    #[serde(rename = "elapsedMs")]
    elapsed_ms: u128,
    #[serde(rename = "scannedAt")]
    scanned_at: String,
    #[serde(rename = "dupeMinSize")]
    dupe_min_size: u64,
    #[serde(rename = "maxFullHash")]
    max_full_hash: u64,
    version: &'a str,
}

#[derive(Serialize)]
struct JsonDupes {
    absolute: Vec<JsonGroup>,
    #[serde(rename = "likelySample")]
    likely_sample: Vec<JsonGroup>,
    #[serde(rename = "likelyName")]
    likely_name: Vec<JsonGroup>,
    #[serde(rename = "wastedAbsolute")]
    wasted_absolute: u64,
    #[serde(rename = "wastedLikely")]
    wasted_likely: u64,
    #[serde(rename = "groupsAbsolute")]
    groups_absolute: usize,
    #[serde(rename = "groupsLikely")]
    groups_likely: usize,
    enabled: bool,
}

#[derive(Serialize)]
struct ReportData<'a> {
    meta: Meta<'a>,
    tree: Node,
    #[serde(rename = "topFiles")]
    top_files: Vec<PathSize>,
    #[serde(rename = "topDirs")]
    top_dirs: Vec<PathSize>,
    dupes: JsonDupes,
}

pub fn generate(
    scan: &ScanResult,
    dupes: Option<&DupeReport>,
    dupe_min_size: u64,
    max_full_hash: u64,
    out_path: &Path,
) -> std::io::Result<()> {
    // Children map: dir -> child dirs.
    let mut children: HashMap<&Path, Vec<&PathBuf>> = HashMap::new();
    for dir in scan.dir_sizes.keys() {
        if dir == &scan.root {
            continue;
        }
        if let Some(parent) = dir.parent() {
            children.entry(parent).or_default().push(dir);
        }
    }

    let min_node = std::cmp::max(scan.total_size / 5000, 32 * 1024);
    let mut budget = MAX_TREE_NODES;
    let tree = build_node(&scan.root, root_label(&scan.root), scan, &children, min_node, 0, &mut budget);

    // Top directories (excluding the root itself).
    let mut dirs: Vec<(&PathBuf, u64)> = scan
        .dir_sizes
        .iter()
        .filter(|(p, _)| *p != &scan.root)
        .map(|(p, s)| (p, *s))
        .collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1));
    let top_dirs: Vec<PathSize> = dirs
        .iter()
        .take(MAX_TOP)
        .map(|(p, s)| PathSize {
            p: p.display().to_string(),
            s: *s,
        })
        .collect();

    let mut file_idx: Vec<usize> = (0..scan.files.len()).collect();
    file_idx.sort_by(|&a, &b| scan.files[b].size.cmp(&scan.files[a].size));
    let top_files: Vec<PathSize> = file_idx
        .iter()
        .take(MAX_TOP)
        .map(|&i| PathSize {
            p: scan.files[i].path.display().to_string(),
            s: scan.files[i].size,
        })
        .collect();

    let json_dupes = match dupes {
        Some(d) => JsonDupes {
            absolute: d.absolute.iter().take(MAX_GROUPS).map(to_json_group).collect(),
            likely_sample: d.likely_sample.iter().take(MAX_GROUPS).map(to_json_group).collect(),
            likely_name: d.likely_name.iter().take(MAX_GROUPS).map(to_json_group).collect(),
            wasted_absolute: d.wasted_absolute(),
            wasted_likely: d.wasted_likely(),
            groups_absolute: d.absolute.len(),
            groups_likely: d.likely_sample.len() + d.likely_name.len(),
            enabled: true,
        },
        None => JsonDupes {
            absolute: vec![],
            likely_sample: vec![],
            likely_name: vec![],
            wasted_absolute: 0,
            wasted_likely: 0,
            groups_absolute: 0,
            groups_likely: 0,
            enabled: false,
        },
    };

    let data = ReportData {
        meta: Meta {
            root: scan.root.display().to_string(),
            total_size: scan.total_size,
            file_count: scan.files.len() as u64,
            dir_count: scan.dir_count,
            error_count: scan.error_count,
            elapsed_ms: scan.elapsed.as_millis(),
            scanned_at: crate::units::now_utc_string(),
            dupe_min_size,
            max_full_hash,
            version: env!("CARGO_PKG_VERSION"),
        },
        tree,
        top_files,
        top_dirs,
        dupes: json_dupes,
    };

    let json = serde_json::to_string(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
        // Keep the embedded JSON from terminating the <script> block early.
        .replace("</", "<\\/");

    let html = include_str!("template.html").replace("__SPACE_INVADER_DATA__", &json);
    std::fs::write(out_path, html)
}

fn root_label(root: &Path) -> String {
    let s = root.display().to_string();
    if s.is_empty() {
        "(root)".into()
    } else {
        s
    }
}

fn build_node(
    dir: &Path,
    name: String,
    scan: &ScanResult,
    children: &HashMap<&Path, Vec<&PathBuf>>,
    min_node: u64,
    depth: usize,
    budget: &mut usize,
) -> Node {
    let size = scan.dir_sizes.get(dir).copied().unwrap_or(0);
    let mut node = Node {
        n: name,
        s: size,
        c: vec![],
    };
    if depth >= MAX_DEPTH || *budget == 0 {
        return node;
    }
    if let Some(kids) = children.get(dir) {
        let mut kids: Vec<(&PathBuf, u64)> = kids
            .iter()
            .map(|p| (*p, scan.dir_sizes.get(*p).copied().unwrap_or(0)))
            .collect();
        kids.sort_by(|a, b| b.1.cmp(&a.1));
        for (child, csize) in kids.into_iter().take(MAX_CHILDREN) {
            if csize < min_node || *budget == 0 {
                break;
            }
            *budget -= 1;
            let cname = child
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| child.display().to_string());
            node.c
                .push(build_node(child, cname, scan, children, min_node, depth + 1, budget));
        }
    }
    node
}

fn to_json_group(g: &DupeGroup) -> JsonGroup {
    JsonGroup {
        sz: g.size,
        w: g.wasted,
        k: g.key.clone(),
        f: g.files.iter().take(MAX_FILES_PER_GROUP).cloned().collect(),
        truncated: g.files.len() > MAX_FILES_PER_GROUP,
    }
}
