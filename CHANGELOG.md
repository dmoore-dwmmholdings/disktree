# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **MFT fast scan for elevated NTFS volumes.** Instead of one metadata read per
  directory, the whole volume's file table is read in a streaming pass. Used
  automatically when the volume is NTFS, the raw device is readable, and the
  target is the volume root; `--mft` forces it for a subdirectory and `--no-mft`
  disables it. Anything else falls back to the directory walk with a one-line
  explanation rather than an error.
- **Automatic elevation.** A whole-volume scan without privileges relaunches
  itself elevated in a new window, since a console process cannot gain
  privileges in place. `--no-elevate` opts out, and the relaunched process
  carries an internal flag so a failed probe cannot produce an elevation loop.
- `--mft-readers` for read concurrency during an MFT scan, defaulting to 8.
  Measured on the development NVMe drive, unbuffered 16 MiB reads scale 1774 →
  3613 MB/s from 1 to 8 concurrent readers and plateau there.
- `examples/mftprobe.rs`, a go/no-go check that runs the whole privileged
  sequence with no parser attached, so a failure names one API. Also reports
  bandwidth against concurrency.
- `examples/iocal.rs`, which calibrates read concurrency on any machine without
  needing elevation, and `examples/crosscheck.rs`, which scans a subtree both
  ways and diffs every shared directory.

### Notes on correctness

The MFT parser's size and structure logic is pure functions over byte slices,
covered by 17 tests driving synthetic records. Two of the cases fail silently if
got wrong and are worth naming: resident `$DATA` must be sized from the
attribute's value length, or every small file — most files by count — reports
zero bytes; and `$ATTRIBUTE_LIST` spillover must be merged into the base record,
or the largest fragmented files report zero. Also covered: DOS 8.3 aliases
deduplicated, continuation extents ignored, alternate streams excluded, torn
reads and `BAAD` records rejected, malformed attribute chains terminating,
random bytes never panicking, and stale or missing parent references attaching
to an orphan container rather than to a recycled directory slot.

The two scanners do not report identical totals, by design. Hard-linked files
are counted once per link by the walk and once in total by the MFT, so the walk
reports more for trees like `C:\Windows`. `crosscheck` quantifies the delta.

## [0.2.0] - 2026-07-26

### Fixed

- **The declared MSRV was wrong and the crate did not build on it.** `0.1.0`
  declared `rust-version = "1.85"` but used `is_multiple_of` (stable in 1.87)
  and let-chains (stable in 1.88), so anyone on 1.85 or 1.86 passed cargo's
  version check and then hit a compile error. The declared minimum is now 1.88
  and is verified against that toolchain.
- **Symlinks, junctions, and volume mount points vanished from the tree.** They
  were skipped entirely rather than recorded, so a volume mounted into a scanned
  directory was absent instead of showing as empty, and file counts undercounted.
  They now appear as zero-byte entries and are still never traversed.
- **A failed metadata read was reported as a zero-byte file.** It now counts
  toward the unreadable total the header displays.
- **The delete confirmation could not be acted on safely.** It listed bare names,
  so marking three unrelated `bin` folders read as "bin, bin, bin". It now shows
  full paths.
- **A partly completed delete left every ancestor overstating its size.**
  `remove_dir_all` can delete most of a tree before hitting an open file, and the
  Recycle Bin can accept some items and abort others; only the success path
  reconciled the tree. Failures are now re-checked on disk, and totals known to
  be stale say so.
- **Deleting something already gone reported freed bytes that were never freed.**
  Targets are re-checked immediately before deletion, since the tree can be
  minutes old.
- Recycle Bin quota failures reported the unhelpful "Some operations were
  aborted"; they now say what happened and suggest permanent deletion.
- Off Windows, running with no arguments failed with `'C:\' is not a directory`.
  The default root is now `/` there.

### Changed

- **Rendering cost no longer grows with directory size.** The table built one
  widget per child, which took roughly half a second per frame at 100,000
  entries — reachable today, since `C:\Windows\WinSxS\Manifests` holds tens of
  thousands. Only the visible rows are built now: measured 3.007 ms per frame at
  1,000 children versus 3.110 ms at 100,000.
- Directory enumeration runs on its own thread pool sized at twice the core
  count, leaving rayon's global pool for CPU-bound work. `--threads`/`-j`
  overrides it.
- Progress counters are published once per directory instead of once per file,
  and the displayed current path is sampled rather than formatted for every
  directory.

### Added

- `--threads`/`-j` to set the number of concurrent directory reads.
- `examples/bench.rs`, a scan benchmark reporting files/s and dirs/s, with a
  `--split` mode for A/B comparisons against a single-use cold cache.
- `examples/probe.rs`, which reports which volume access masks and USN ioctls the
  current privilege level allows — the groundwork for an MFT-based fast scan.
- Crate-level documentation with a running doctest, docs for the whole `index`
  API, and a README section explaining exactly what the reported sizes include.

### Notes

`app` and `ui` are now `#[doc(hidden)]`. Their signatures name `ratatui` types,
so treating them as public API would make every `ratatui` release a breaking
change here. `index` is the supported surface.

## [0.1.0] - 2026-07-26

Initial release: parallel directory scan with live progress, size-sorted
browser with percentage bars, and deletion to the Recycle Bin or permanently,
behind a confirmation.
