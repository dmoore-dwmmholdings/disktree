use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use rayon::prelude::*;

/// One directory in every this many refreshes the displayed "current path".
const PATH_SAMPLE_INTERVAL: u64 = 1024;

/// Shared counters updated live by the scan threads and read by the UI.
#[derive(Default)]
pub struct ScanProgress {
    pub dirs: AtomicU64,
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    pub current: Mutex<String>,
}

/// A single file or directory in the scanned tree (arena storage, indexed by `usize`).
pub struct Node {
    pub name: OsString,
    pub is_dir: bool,
    pub size: u64,
    pub n_files: u64,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub deleted: bool,
}

pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: usize,
    pub root_path: PathBuf,
    pub errors: u64,
    pub scan_secs: f64,
}

impl Tree {
    /// Reconstruct the full filesystem path of a node.
    pub fn path_of(&self, mut idx: usize) -> PathBuf {
        let mut parts: Vec<&OsString> = Vec::new();
        while let Some(parent) = self.nodes[idx].parent {
            parts.push(&self.nodes[idx].name);
            idx = parent;
        }
        let mut path = self.root_path.clone();
        for part in parts.iter().rev() {
            path.push(part);
        }
        path
    }

    /// Detach a node from the tree and subtract its size/counts from all ancestors.
    pub fn remove(&mut self, idx: usize) {
        self.nodes[idx].deleted = true;
        let size = self.nodes[idx].size;
        let n_files = self.nodes[idx].n_files;
        if let Some(parent) = self.nodes[idx].parent {
            self.nodes[parent].children.retain(|&c| c != idx);
            let mut ancestor = Some(parent);
            while let Some(i) = ancestor {
                self.nodes[i].size -= size;
                self.nodes[i].n_files -= n_files;
                ancestor = self.nodes[i].parent;
            }
        }
    }

    /// True if this node or any of its ancestors has been deleted.
    pub fn is_gone(&self, mut idx: usize) -> bool {
        loop {
            if self.nodes[idx].deleted {
                return true;
            }
            match self.nodes[idx].parent {
                Some(p) => idx = p,
                None => return false,
            }
        }
    }
}

struct ScanEntry {
    name: OsString,
    is_dir: bool,
    size: u64,
    n_files: u64,
    children: Vec<ScanEntry>,
}

/// Threads to use for directory enumeration when the caller does not choose.
///
/// Cold-cache scanning is bound on directory-index reads rather than CPU: a
/// worker sits blocked in the kernel instead of computing, so one thread per
/// core can leave the device queue short of requests. Hence 2x cores.
///
/// It is deliberately only 2x. A larger pool is defensible in theory, but on the
/// development machine it could not be shown to help: warm throughput is flat
/// from 16 to 128 threads, and a cold comparison was not obtainable (with 64 GB
/// of RAM the NTFS metadata cache never evicts, and dropping it needs
/// privileges). Absent evidence, this stays near the measured-flat region and
/// `--threads` exists for anyone whose workload differs.
fn default_io_threads() -> usize {
    let cpus = std::thread::available_parallelism().map_or(8, |n| n.get());
    (cpus * 2).clamp(8, 64)
}

/// Build the pool used for directory enumeration.
///
/// Deliberately separate from rayon's global pool: the CPU-bound aggregation
/// work wants one thread per core, and this one is intentionally oversubscribed.
fn io_pool(threads: Option<usize>) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.unwrap_or_else(default_io_threads))
        // Recursion depth follows tree depth; this is reserved address space,
        // not committed memory.
        .stack_size(4 << 20)
        .build()
        .expect("build scan thread pool")
}

/// Scan `root` recursively (in parallel) and return the flattened tree.
pub fn scan(root: PathBuf, progress: &ScanProgress) -> Tree {
    scan_with_threads(root, progress, None)
}

/// [`scan`], with an explicit enumeration thread count. `None` picks a default.
pub fn scan_with_threads(root: PathBuf, progress: &ScanProgress, threads: Option<usize>) -> Tree {
    let start = Instant::now();
    let root_name = OsString::from(root.display().to_string());
    let pool = io_pool(threads);
    let entry = pool.install(|| scan_dir(root_name, &root, progress));
    let scan_secs = start.elapsed().as_secs_f64();

    let expected =
        1 + progress.dirs.load(Ordering::Relaxed) + progress.files.load(Ordering::Relaxed);
    let mut nodes: Vec<Node> = Vec::with_capacity(expected as usize);
    nodes.push(Node {
        name: entry.name,
        is_dir: true,
        size: entry.size,
        n_files: entry.n_files,
        parent: None,
        children: Vec::new(),
        deleted: false,
    });

    let mut stack: Vec<(usize, Vec<ScanEntry>)> = vec![(0, entry.children)];
    while let Some((parent_idx, children)) = stack.pop() {
        for child in children {
            let idx = nodes.len();
            nodes.push(Node {
                name: child.name,
                is_dir: child.is_dir,
                size: child.size,
                n_files: child.n_files,
                parent: Some(parent_idx),
                children: Vec::new(),
                deleted: false,
            });
            nodes[parent_idx].children.push(idx);
            if !child.children.is_empty() {
                stack.push((idx, child.children));
            }
        }
    }

    Tree {
        nodes,
        root: 0,
        root_path: root,
        errors: progress.errors.load(Ordering::Relaxed),
        scan_secs,
    }
}

fn scan_dir(name: OsString, path: &Path, progress: &ScanProgress) -> ScanEntry {
    // Counters are accumulated locally and published once per directory. Doing
    // it per entry puts 32 workers on the same cache line for no extra fidelity.
    let mut n_files_here = 0u64;
    let mut bytes_here = 0u64;
    let mut errors_here = 0u64;

    let dirs_seen = progress.dirs.fetch_add(1, Ordering::Relaxed);
    // Best-effort telemetry, sampled: this feeds a UI that redraws ~10x/sec, so
    // formatting every path would allocate far more than anyone can read.
    if dirs_seen % PATH_SAMPLE_INTERVAL == 0
        && let Ok(mut current) = progress.current.try_lock()
    {
        *current = path.display().to_string();
    }

    let mut children: Vec<ScanEntry> = Vec::new();
    let mut subdirs: Vec<(OsString, PathBuf)> = Vec::new();

    let read_dir = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => {
            progress.errors.fetch_add(1, Ordering::Relaxed);
            return ScanEntry {
                name,
                is_dir: true,
                size: 0,
                n_files: 0,
                children,
            };
        }
    };

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                errors_here += 1;
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => {
                errors_here += 1;
                continue;
            }
        };

        // Reparse points that are name surrogates (symlinks, junctions, volume
        // mount points) are recorded as zero-byte entries but never traversed:
        // following them would double-count or cycle. They must still appear as
        // nodes, otherwise a volume mounted under the scan root vanishes from
        // the tree entirely rather than showing as empty.
        //
        // The test is deliberately `is_symlink()`, which std implements as
        // "reparse point AND name-surrogate bit". A blanket
        // FILE_ATTRIBUTE_REPARSE_POINT check would also skip Dropbox/OneDrive
        // and dedup-backed files, whose reparse tags are not surrogates and
        // whose contents must be counted.
        if file_type.is_symlink() {
            children.push(ScanEntry {
                name: entry.file_name(),
                is_dir: false,
                size: 0,
                n_files: 0,
                children: Vec::new(),
            });
            continue;
        }

        if file_type.is_dir() {
            subdirs.push((entry.file_name(), entry.path()));
        } else {
            // On Windows this is served from the cached directory entry and
            // cannot fail; on other platforms it can, and a silent 0 would be
            // reported as a real size, so count it.
            let size = match entry.metadata() {
                Ok(m) => m.len(),
                Err(_) => {
                    errors_here += 1;
                    0
                }
            };
            n_files_here += 1;
            bytes_here += size;
            children.push(ScanEntry {
                name: entry.file_name(),
                is_dir: false,
                size,
                n_files: 1,
                children: Vec::new(),
            });
        }
    }

    if n_files_here > 0 {
        progress.files.fetch_add(n_files_here, Ordering::Relaxed);
        progress.bytes.fetch_add(bytes_here, Ordering::Relaxed);
    }
    if errors_here > 0 {
        progress.errors.fetch_add(errors_here, Ordering::Relaxed);
    }

    let mut scanned_dirs: Vec<ScanEntry> = subdirs
        .into_par_iter()
        .map(|(name, path)| scan_dir(name, &path, progress))
        .collect();
    children.append(&mut scanned_dirs);

    let size = children.iter().map(|c| c.size).sum();
    let n_files = children.iter().map(|c| c.n_files).sum();
    ScanEntry {
        name,
        is_dir: true,
        size,
        n_files,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn build_fixture(root: &Path) {
        fs::create_dir_all(root.join("sub").join("inner")).unwrap();
        File::create(root.join("a.txt"))
            .unwrap()
            .write_all(&[0u8; 100])
            .unwrap();
        File::create(root.join("sub").join("b.txt"))
            .unwrap()
            .write_all(&[0u8; 2048])
            .unwrap();
        File::create(root.join("sub").join("inner").join("c.txt"))
            .unwrap()
            .write_all(&[0u8; 1])
            .unwrap();
    }

    fn fixture_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rustree_test_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        build_fixture(&root);
        root
    }

    fn child_by_name(tree: &Tree, parent: usize, name: &str) -> usize {
        *tree.nodes[parent]
            .children
            .iter()
            .find(|&&c| tree.nodes[c].name == name)
            .unwrap()
    }

    #[test]
    fn scan_aggregates_sizes() {
        let root = fixture_root("scan");
        let progress = ScanProgress::default();
        let tree = scan(root.clone(), &progress);

        assert_eq!(tree.nodes[tree.root].size, 2149);
        assert_eq!(tree.nodes[tree.root].n_files, 3);

        let sub = child_by_name(&tree, tree.root, "sub");
        assert_eq!(tree.nodes[sub].size, 2049);
        assert_eq!(tree.nodes[sub].n_files, 2);

        let inner = child_by_name(&tree, sub, "inner");
        assert_eq!(tree.path_of(inner), root.join("sub").join("inner"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn remove_propagates_up() {
        let root = fixture_root("remove");
        let progress = ScanProgress::default();
        let mut tree = scan(root.clone(), &progress);

        let sub = child_by_name(&tree, tree.root, "sub");
        let inner = child_by_name(&tree, sub, "inner");
        tree.remove(inner);

        assert_eq!(tree.nodes[sub].size, 2048);
        assert_eq!(tree.nodes[sub].n_files, 1);
        assert_eq!(tree.nodes[tree.root].size, 2148);
        assert_eq!(tree.nodes[tree.root].n_files, 2);
        assert!(tree.is_gone(inner));
        assert!(!tree.is_gone(sub));

        fs::remove_dir_all(&root).unwrap();
    }
}
