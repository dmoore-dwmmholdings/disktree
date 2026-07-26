//! Scan benchmark: `cargo run --release --example bench -- <path> [runs]`
//!
//! Reports per-run wall time plus files/sec and total bytes so scanner changes
//! can be compared on the same tree.

use std::path::PathBuf;
use std::time::Instant;

use disktree::index::{ScanProgress, scan};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = PathBuf::from(args.next().unwrap_or_else(|| "C:\\".to_owned()));
    let runs: usize = args.next().and_then(|r| r.parse().ok()).unwrap_or(3);

    println!("target: {}", path.display());
    let mut times = Vec::new();
    for i in 1..=runs {
        let progress = ScanProgress::default();
        let start = Instant::now();
        let tree = scan(path.clone(), &progress);
        let secs = start.elapsed().as_secs_f64();
        times.push(secs);
        let root = &tree.nodes[tree.root];
        println!(
            "run {i}: {secs:.3} s  |  {} files  {} nodes  {:.2} GB  |  {:.0} files/s  |  {} errors",
            root.n_files,
            tree.nodes.len(),
            root.size as f64 / 1e9,
            root.n_files as f64 / secs,
            tree.errors,
        );
    }
    let best = times.iter().copied().fold(f64::MAX, f64::min);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    println!("best: {best:.3} s   mean: {mean:.3} s");
}
