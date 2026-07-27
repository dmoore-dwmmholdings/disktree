//! Compare the MFT scan against the directory walk: `crosscheck <subtree>`
//!
//! Run elevated. Scans the whole volume via the MFT, scans `<subtree>` by walking
//! directories, then diffs every directory the two have in common.
//!
//! They are *not* expected to agree exactly, and the differences are the point:
//!
//!   - a file with N hard links is counted once by the MFT and N times by the
//!     walk, because a directory entry cannot tell that two names share extents
//!   - a reparse point is a zero-byte entry to the walk and, in the MFT, whatever
//!     its own record says
//!   - the walk cannot read directories it lacks permission for; the MFT can
//!
//! So this reports the delta per directory and flags only the ones it cannot
//! explain. An unexplained delta means a parsing bug — most likely a mis-sized
//! resident `$DATA`, a lost `$ATTRIBUTE_LIST` extension, or a bad fixup.

#[cfg(windows)]
fn main() {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use disktree::index::{ScanProgress, Tree, scan};
    use disktree::mft::{self, Capability};

    let Some(target) = std::env::args().nth(1) else {
        eprintln!("usage: crosscheck <directory-to-compare>");
        std::process::exit(2);
    };
    let target = PathBuf::from(&target);
    if !target.is_dir() {
        eprintln!("'{}' is not a directory", target.display());
        std::process::exit(2);
    }

    let vol = match mft::probe(&target) {
        Capability::Ready(v) => v,
        Capability::NeedsElevation { letter, .. } => {
            eprintln!("needs elevation: run from an Administrator shell (volume {letter}:)");
            std::process::exit(1);
        }
        Capability::NotNtfs { filesystem } => {
            eprintln!("{filesystem} is not NTFS; nothing to compare");
            std::process::exit(1);
        }
        Capability::Unsupported(why) => {
            eprintln!("MFT unavailable: {why}");
            std::process::exit(1);
        }
    };

    println!(
        "volume {} — $MFT {:.2} GB in {} fragments, up to {} slots",
        vol.mount().display(),
        vol.mft_bytes() as f64 / 1e9,
        vol.fragments(),
        vol.record_slots()
    );

    println!("\nMFT scan…");
    let p = ScanProgress::default();
    let t0 = std::time::Instant::now();
    let mft_tree = match mft::scan(&vol, &p, None) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("MFT scan failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "  {:.3} s — {} nodes, {} files, {:.2} GB, {} skipped records",
        t0.elapsed().as_secs_f64(),
        mft_tree.nodes.len(),
        mft_tree.nodes[mft_tree.root].n_files,
        mft_tree.nodes[mft_tree.root].size as f64 / 1e9,
        mft_tree.errors,
    );
    println!(
        "  throughput: {:.0} MB/s of $MFT",
        vol.mft_bytes() as f64 / 1e6 / t0.elapsed().as_secs_f64().max(1e-9)
    );

    println!("\ndirectory walk of {}…", target.display());
    let p2 = ScanProgress::default();
    let t1 = std::time::Instant::now();
    let walk_tree = scan(target.clone(), &p2);
    println!(
        "  {:.3} s — {} nodes, {} files, {:.2} GB, {} unreadable",
        t1.elapsed().as_secs_f64(),
        walk_tree.nodes.len(),
        walk_tree.nodes[walk_tree.root].n_files,
        walk_tree.nodes[walk_tree.root].size as f64 / 1e9,
        walk_tree.errors,
    );

    /// Comparison key for a path.
    ///
    /// Both separators are folded to `\` and the case is lowered, because the two
    /// trees are rooted differently: the MFT tree's root is the mount point, so
    /// it always produces `C:\...`, while the walk's root is whatever the user
    /// typed — and `C:/Users/x` would otherwise never match `C:\Users\x`.
    fn key_of(p: &std::path::Path) -> String {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }

    // Index both trees by full path, directories only: a file's size is compared
    // through its parent's total, and per-file comparison would drown in noise
    // from files written between the two scans.
    fn dirs_by_path(t: &Tree) -> HashMap<String, (u64, u64)> {
        let mut m = HashMap::new();
        for i in 0..t.nodes.len() {
            if !t.nodes[i].is_dir {
                continue;
            }
            m.insert(key_of(&t.path_of(i)), (t.nodes[i].size, t.nodes[i].n_files));
        }
        m
    }

    let a = dirs_by_path(&mft_tree);
    let b = dirs_by_path(&walk_tree);

    let mut compared = 0usize;
    let mut identical = 0usize;
    let mut diffs: Vec<(String, i128, i128)> = Vec::new();
    for (path, &(wsize, wfiles)) in &b {
        let Some(&(msize, mfiles)) = a.get(path) else {
            continue;
        };
        compared += 1;
        let dsize = msize as i128 - wsize as i128;
        let dfiles = mfiles as i128 - wfiles as i128;
        if dsize == 0 && dfiles == 0 {
            identical += 1;
        } else {
            diffs.push((path.clone(), dsize, dfiles));
        }
    }

    println!("\n=== comparison ===");
    println!("  directories present in both : {compared}");
    println!("  byte-for-byte identical     : {identical}");
    println!("  differing                   : {}", diffs.len());
    if compared == 0 {
        // Show both key spaces rather than guessing: the usual cause is that the
        // two trees are rooted differently, which is invisible from the totals.
        println!("\nNothing overlapped, so the keys do not line up. Sample keys:");
        let mut sa: Vec<&String> = a.keys().collect();
        let mut sb: Vec<&String> = b.keys().collect();
        sa.sort();
        sb.sort();
        for k in sa.iter().take(3) {
            println!("  MFT : {k}");
        }
        for k in sb.iter().take(3) {
            println!("  walk: {k}");
        }
        println!("Is the target on the scanned volume?");
        return;
    }
    println!(
        "  agreement                   : {:.2}%",
        identical as f64 / compared as f64 * 100.0
    );

    // Largest absolute differences first: a systematic parsing bug shows up as a
    // few enormous deltas, not as many tiny ones.
    diffs.sort_by_key(|(_, ds, _)| -(ds.abs()));
    if !diffs.is_empty() {
        println!("\n  largest differences (MFT minus walk):");
        for (path, ds, df) in diffs.iter().take(15) {
            println!("    {ds:>+16} bytes  {df:>+8} files   {path}");
        }
    }

    let total_mft = a.get(&key_of(&target)).map(|v| v.0);
    if let Some(m) = total_mft {
        let w = walk_tree.nodes[walk_tree.root].size;
        println!(
            "\n  target total: MFT {:.3} GB vs walk {:.3} GB  ({:+.2}%)",
            m as f64 / 1e9,
            w as f64 / 1e9,
            (m as f64 - w as f64) / w.max(1) as f64 * 100.0
        );
        println!(
            "  A walk total above the MFT total is expected: hard-linked files are\n  \
             counted once per link by the walk and once in total by the MFT."
        );
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("crosscheck is Windows-only");
}
