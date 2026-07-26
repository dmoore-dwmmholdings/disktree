//! Scan benchmark.
//!
//! ```text
//! bench <path> [runs]                      # repeated scans, best/mean
//! bench <path> --threads N [runs]          # fix the I/O pool size
//! bench <path> --split A,B                 # cold-safe A/B of two pool sizes
//! ```
//!
//! `--split` exists because a cold cache is single-use: once a tree is read,
//! every later run is warm and ~77x faster, so two sequential runs can never be
//! compared. It partitions the target's top-level children into two halves of
//! similar size and scans each with a different pool size in ONE process, so a
//! single cold state yields a valid pair. Check the reported node counts agree
//! before trusting the times.

use std::path::{Path, PathBuf};
use std::time::Instant;

use disktree::index::{ScanProgress, scan_with_threads};

struct Row {
    label: String,
    secs: f64,
    files: u64,
    dirs: u64,
    nodes: usize,
    bytes: u64,
}

fn report(rows: &[Row]) {
    for r in rows {
        println!(
            "  {:<14} {:>8.3} s | {:>9} files {:>8} dirs {:>9} nodes | {:>9.0} files/s {:>8.0} dirs/s | {:.2} GB",
            r.label,
            r.secs,
            r.files,
            r.dirs,
            r.nodes,
            r.files as f64 / r.secs,
            r.dirs as f64 / r.secs,
            r.bytes as f64 / 1e9,
        );
    }
}

fn run_one(path: &Path, threads: Option<usize>) -> Row {
    let progress = ScanProgress::default();
    let start = Instant::now();
    let tree = scan_with_threads(path.to_path_buf(), &progress, threads);
    let secs = start.elapsed().as_secs_f64();
    let root = &tree.nodes[tree.root];
    Row {
        label: match threads {
            Some(n) => format!("threads={n}"),
            None => "threads=default".to_owned(),
        },
        secs,
        files: root.n_files,
        dirs: tree.nodes.iter().filter(|n| n.is_dir).count() as u64,
        nodes: tree.nodes.len(),
        bytes: root.size,
    }
}

/// Split the target's immediate subdirectories into two groups of similar total
/// entry count. Deterministic: sort by name, then greedily assign each to
/// whichever group is currently smaller, using a cheap one-level entry count as
/// the weight. No recursion here, so this stays cheap and does not warm the tree.
fn split_children(path: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut kids: Vec<PathBuf> = std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_type()
                .map(|t| t.is_dir() && !t.is_symlink())
                .unwrap_or(false)
        })
        .map(|e| e.path())
        .collect();
    kids.sort();

    let weights: Vec<usize> = kids
        .iter()
        .map(|k| std::fs::read_dir(k).into_iter().flatten().count().max(1))
        .collect();

    let mut order: Vec<usize> = (0..kids.len()).collect();
    order.sort_by(|&a, &b| weights[b].cmp(&weights[a]));

    let (mut a, mut b) = (Vec::new(), Vec::new());
    let (mut wa, mut wb) = (0usize, 0usize);
    for i in order {
        if wa <= wb {
            wa += weights[i];
            a.push(kids[i].clone());
        } else {
            wb += weights[i];
            b.push(kids[i].clone());
        }
    }
    (a, b)
}

fn scan_group(group: &[PathBuf], threads: usize, label: &str) -> Row {
    let progress = ScanProgress::default();
    let start = Instant::now();
    let mut files = 0;
    let mut dirs = 0;
    let mut nodes = 0;
    let mut bytes = 0;
    for p in group {
        let tree = scan_with_threads(p.clone(), &progress, Some(threads));
        let root = &tree.nodes[tree.root];
        files += root.n_files;
        bytes += root.size;
        nodes += tree.nodes.len();
        dirs += tree.nodes.iter().filter(|n| n.is_dir).count() as u64;
    }
    Row {
        label: label.to_owned(),
        secs: start.elapsed().as_secs_f64(),
        files,
        dirs,
        nodes,
        bytes,
    }
}

fn main() {
    // Parsed positionally with explicit flag-value skipping. An earlier version
    // scanned for "the first numeric argument", which happily consumed the value
    // of --threads as the run count and silently turned one cold run into N-1
    // warm ones.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut path: Option<String> = None;
    let mut runs: Option<usize> = None;
    let mut threads_arg: Option<String> = None;
    let mut split_arg: Option<String> = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--threads" => {
                threads_arg = argv.get(i + 1).cloned();
                i += 2;
            }
            "--split" => {
                split_arg = argv.get(i + 1).cloned();
                i += 2;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag: {other}");
                std::process::exit(2);
            }
            other => {
                if path.is_none() {
                    path = Some(other.to_owned());
                } else if runs.is_none() {
                    match other.parse() {
                        Ok(n) => runs = Some(n),
                        Err(_) => {
                            eprintln!("expected a run count, got: {other}");
                            std::process::exit(2);
                        }
                    }
                } else {
                    eprintln!("unexpected argument: {other}");
                    std::process::exit(2);
                }
                i += 1;
            }
        }
    }
    let path = PathBuf::from(path.unwrap_or_else(|| "C:\\".to_owned()));
    let runs = runs.unwrap_or(3);
    let flag = |name: &str| -> Option<String> {
        match name {
            "--threads" => threads_arg.clone(),
            "--split" => split_arg.clone(),
            _ => None,
        }
    };

    println!("target: {}", path.display());

    if let Some(spec) = flag("--split") {
        let sizes: Vec<usize> = spec
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        let [ta, tb] = match sizes[..] {
            [a, b] => [a, b],
            _ => {
                eprintln!("--split needs exactly two thread counts, e.g. --split 32,128");
                std::process::exit(1);
            }
        };
        let (ga, gb) = split_children(&path);
        println!(
            "split: {} dirs vs {} dirs (one cold pass each)",
            ga.len(),
            gb.len()
        );
        let ra = scan_group(&ga, ta, &format!("A threads={ta}"));
        let rb = scan_group(&gb, tb, &format!("B threads={tb}"));
        report(&[ra, rb]);
        println!("compare dirs/s. Node counts must be within ~5% for the pair to be fair.");
        return;
    }

    let threads = flag("--threads").and_then(|t| t.parse::<usize>().ok());
    let mut rows = Vec::new();
    for _ in 0..runs {
        rows.push(run_one(&path, threads));
    }
    report(&rows);
    let best = rows.iter().map(|r| r.secs).fold(f64::MAX, f64::min);
    let mean = rows.iter().map(|r| r.secs).sum::<f64>() / rows.len() as f64;
    println!("best: {best:.3} s   mean: {mean:.3} s");
}
