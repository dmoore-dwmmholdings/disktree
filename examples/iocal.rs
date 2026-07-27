//! Read-bandwidth calibration: `cargo run --release --example iocal -- <big-file>`
//!
//! Measures unbuffered read throughput against the number of concurrent readers.
//! Needs no elevation: `FILE_FLAG_NO_BUFFERING` bypasses the cache, so reading a
//! large ordinary file exercises the device the same way the MFT scan does.
//!
//! Use it to choose `--mft-readers` on hardware other than the developer's.

#[cfg(windows)]
fn main() {
    let Some(file) = std::env::args().nth(1) else {
        eprintln!("usage: iocal <path-to-file-of-at-least-512MB>");
        std::process::exit(2);
    };
    let path = std::path::PathBuf::from(&file);
    let levels = [1usize, 2, 4, 8, 16, 32];
    match disktree::mft::calibrate_readers(&path, &levels) {
        Ok(rows) => {
            println!("target: {file}\n");
            println!(" readers   MB/s   relative");
            let base = rows.first().map(|r| r.1).unwrap_or(1.0);
            for (readers, bps) in &rows {
                println!(
                    "  {readers:>6}  {:>6.0}   {:>5.2}x",
                    bps / 1e6,
                    bps / base.max(1.0)
                );
            }
            if let Some((best_n, best_bps)) = rows
                .iter()
                .copied()
                .reduce(|a, b| if b.1 > a.1 { b } else { a })
            {
                println!("\nbest: {best_n} readers at {:.0} MB/s", best_bps / 1e6);
            }
        }
        Err(e) => eprintln!("calibration failed: {e}"),
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("iocal is Windows-only");
}
