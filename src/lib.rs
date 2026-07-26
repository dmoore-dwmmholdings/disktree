//! Disk usage scanning for a terminal UI: find what is large, then delete it.
//!
//! This crate backs the `disktree` binary. The reusable part is [`index`], which
//! walks a directory tree in parallel and returns it as a flat arena of
//! [`Node`](index::Node)s addressed by index, with sizes and file counts already
//! rolled up into every directory.
//!
//! ```
//! use disktree::index::{ScanProgress, scan};
//! # use std::fs;
//! # let dir = std::env::temp_dir().join(format!("disktree_doc_{}", std::process::id()));
//! # let _ = fs::remove_dir_all(&dir);
//! # fs::create_dir_all(dir.join("logs")).unwrap();
//! # fs::write(dir.join("logs").join("big.log"), vec![0u8; 4096]).unwrap();
//! # fs::write(dir.join("small.txt"), vec![0u8; 10]).unwrap();
//! let progress = ScanProgress::default();
//! let tree = scan(dir.clone(), &progress);
//!
//! // The root aggregates its whole subtree.
//! let root = &tree.nodes[tree.root];
//! assert_eq!(root.n_files, 2);
//! assert_eq!(root.size, 4106);
//!
//! // Children are indices into `tree.nodes`; sort them to find the biggest.
//! let mut kids = root.children.clone();
//! kids.sort_by_key(|&i| std::cmp::Reverse(tree.nodes[i].size));
//!
//! let biggest = kids[0];
//! assert_eq!(tree.nodes[biggest].name, "logs");
//! assert_eq!(tree.nodes[biggest].size, 4096);
//! assert_eq!(tree.path_of(biggest), dir.join("logs"));
//! # fs::remove_dir_all(&dir).unwrap();
//! ```
//!
//! # What the numbers mean
//!
//! Sizes are **apparent** sizes — the logical length of a file, the same value
//! [`std::fs::Metadata::len`] reports — not the space it occupies on disk. For
//! compressed, sparse, or cloud-placeholder files the two differ, sometimes by a
//! lot. Alternate data streams are not counted. Symlinks, junctions, and volume
//! mount points appear as zero-byte entries and are never followed, so a target
//! reached by several links is counted once, under its real path. A file with
//! multiple hard links is counted once per link, because a directory walk cannot
//! tell that two entries share one extent list.
//!
//! Directories that cannot be read are counted in [`Tree::errors`](index::Tree::errors)
//! and contribute zero rather than aborting the scan.

#![warn(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod index;

// The UI layer is an implementation detail of the binary: its signatures name
// `ratatui` types, so exposing it as documented API would make every ratatui
// version bump a breaking change for this crate.
#[doc(hidden)]
pub mod app;
#[doc(hidden)]
pub mod ui;
