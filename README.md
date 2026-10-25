# disktree

A disk usage analyzer for your terminal, in the spirit of [WizTree](https://diskanalyzer.com/), WinDirStat, and ncdu: scan a drive or folder, see what is actually taking the space, and delete it — without leaving the console.

```
 C:\Users\you  —  762.3 GB, 1,625,175 files  (48 unreadable)
       Size      %                 Items Name
  662.9 GB  87.0% ████████       1,004,904 AppData\
   39.2 GB   5.1% █                 77,498 Documents\
   20.2 GB   2.6%                   59,412 Dropbox\
    9.6 GB   1.3%                  244,164 .rustup\
 ↑↓ move  enter open  backspace up  space mark  c clear  d → recycle bin  D delete forever  r rescan  q quit
```

- **Two scanners**: a parallel directory walk that works anywhere, and on elevated NTFS an MFT reader that streams the whole volume's metadata in one pass
- **Live progress**, and a size-sorted view of every directory
- **Marks and safe deletion**: mark any number of entries, then send them to the **Recycle Bin** or delete permanently, always behind a confirmation that lists the **full path** of everything about to go
- Sizes update in place after a delete — no rescan needed
- Unreadable directories are counted and reported, never fatal
- Constant-cost rendering: a directory with 100,000 entries draws as fast as one with 10

## Install

```bash
cargo install disktree
```

Requires Rust 1.88 or newer. Windows is the primary target; the scanner and TUI also build and run on Linux and macOS.

## Usage

```bash
disktree                     # scan the system root (C:\ on Windows, / elsewhere)
```

```bash
disktree --path D:\Games     # scan a specific directory
```

```bash
disktree --threads 128       # more concurrent directory reads (see Performance)
```

```bash
disktree --no-mft            # always walk directories, never read the MFT
```

```bash
disktree --no-elevate        # do not relaunch as administrator
```

## Keys

| Key | Action |
|---|---|
| `↑` `↓` or `k` `j` | Move selection |
| `PgUp` `PgDn` | Move by 20 |
| `Home` `End` or `g` `G` | Jump to first / last |
| `Enter` `→` `l` | Open directory |
| `Backspace` `←` `h` | Go up |
| `Space` | Mark / unmark, then advance |
| `c` | Clear all marks |
| `d` | Delete marked (or selected) to the **Recycle Bin** |
| `D` | Delete **permanently** |
| `r` | Rescan |
| `q` | Quit |
| `Esc` | Ask to quit (`Esc` again quits, `Backspace` returns) |

Marks persist while you navigate, so you can gather things from several directories and delete them in one pass. The footer shows how many are marked and their combined size; the confirmation dialog lists full paths, because two folders named `bin` are otherwise indistinguishable.

## Understanding the numbers

Disk usage tools disagree with each other and with Explorer, always for the same handful of reasons. Here is exactly what `disktree` reports, so you can tell a real discrepancy from an expected one.

**Sizes are apparent, not allocated.** A file's size here is its logical length — what `dir` shows and what `std::fs::Metadata::len` returns. It is not the space the file occupies on disk. These differ for:

- **Compressed files** (NTFS compression, or Windows' WOF/"compact" compression used for many system files): apparent size is *larger* than the space used.
- **Sparse files** (VM disk images, some database files): a 100 GB sparse file with 2 GB written reports 100 GB here.
- **Cloud placeholders** (OneDrive/Dropbox "online-only" files): these report their full size while occupying almost nothing locally.
- **Very small files**, which still consume a whole cluster (typically 4 KB) each. A folder of 10,000 one-byte files reports 10 KB and occupies about 40 MB.

WizTree's default column is *allocated* size, so it will not match this tool. That is not a bug in either.

**Hard links are counted once per link by the directory walk, and once in total by the MFT scan.** A directory entry cannot tell that two names share the same data, so the walk contributes a three-link file's size three times. `C:\Windows` uses hard links heavily, which is why the two scanners report different totals for it — see [Do the two scanners agree?](#do-the-two-scanners-agree) below. The MFT scan also attaches a multi-linked file to one parent, so it appears in one directory only.

**Alternate data streams are not counted.** Only the main stream of each file.

**Symlinks, junctions, and mount points are shown as zero-byte entries and never followed.** This is deliberate: following them would double-count, or loop forever. If a whole volume is mounted into a directory inside your scan, it appears as an empty entry rather than being counted at its mount point — scan it by its own path instead. Note that Dropbox and OneDrive folders are *not* in this category and are counted normally.

**Unreadable directories contribute zero.** The header shows a count of them. A scan of `C:\` as a normal user will always have some, so treat the total as a floor.

## Performance

Scanning is bound almost entirely on reading directory metadata from the device, not on CPU. On the development machine (Ryzen 9 7950X3D, NVMe, NTFS), scanning `C:\Users\you\.rustup` — 244,164 files in 8,678 directories:

| Cache state | Time | Throughput |
|---|---|---|
| Cold (first read) | 5.27 s | 46,000 files/s |
| Warm (re-read) | 0.07 s | 3,600,000 files/s |

That 75× spread is the single most important thing to know about any disk-scanner benchmark, including this one: **if you measure twice, the second number is meaningless.** Quoted "files per second" figures for tools of this kind are almost always warm.

Reproduce it with:

```bash
cargo run --release --example bench -- "C:\some\path" 3
```

`--threads` controls how many directories are read concurrently, defaulting to twice your core count. Cold scanning leaves workers blocked on the device rather than busy on the CPU, so a larger pool can raise throughput; on this machine that could not be demonstrated (with 64 GB of RAM the NTFS metadata cache never evicts, and dropping it requires privileges), and warm throughput is flat from 16 to 128 threads. If you scan large cold trees regularly, try `--threads 128` and measure.

### The MFT fast scan

On NTFS, `disktree` can skip directory traversal entirely and read the volume's Master File Table — one large streaming pass over a single file that describes every file on the volume. This is the technique WizTree uses.

It needs Administrator, because raw volume access (`FILE_READ_DATA` on `\\.\C:`) returns `ERROR_ACCESS_DENIED` otherwise. When you scan a whole volume without elevation, `disktree` relaunches itself elevated in a new window (a console process cannot gain privileges in place). Suppress that with `--no-elevate`, and force the walk with `--no-mft`.

The fast path is used when all of these hold, and the directory walk runs otherwise with a one-line explanation in the status bar:

- the volume is NTFS
- the process can read the raw volume
- the requested path is the volume root — the MFT describes an entire volume, so reading it to show one subdirectory would be slower than walking that subdirectory. `--mft` forces it anyway.

Reader concurrency matters more than you might expect. A synchronous read loop keeps one request outstanding, which leaves an NVMe device far short of its sequential peak. Measured on the development machine with unbuffered 16 MiB reads:

| Concurrent readers | Throughput |
|---|---|
| 1 | 1774 MB/s |
| 2 | 3321 MB/s |
| 4 | 3595 MB/s |
| 8 | 3613 MB/s |
| 16 | 3390 MB/s |

The default is 8, on the plateau; `--mft-readers` overrides it. Recalibrate on your own hardware with:

```bash
cargo run --release --example iocal -- <a-file-of-at-least-512MB>
```

### Measured

Full `C:` on the development machine — 2 TB volume, 5.54M files, 6.91 GB of `$MFT` in 35 fragments:

| | |
|---|---|
| Whole-volume MFT scan | **10–21 s** (327–669 MB/s of `$MFT`) |
| Directory walk, cold, of one 1.6M-file subtree | 41 s |

The spread is real: repeated scans of the same volume ranged from 10.3 s to 21.1 s depending on device state, so treat any single figure sceptically — including these. Reader overlap measured 6.3–7.1x, and parsing accounts for only ~2.5 s of it, so this is bound on the device rather than on CPU.

Two caveats. The cost is proportional to the **size of the MFT**, not to how many files are live, so a volume whose MFT holds many dead records pays for them. And a volume with a small MFT sees correspondingly less benefit. Check yours with:

```bash
cargo run --release --example mftprobe -- C
```

### Do the two scanners agree?

Not exactly, and the differences are expected rather than bugs:

- **Hard links.** The MFT counts a file once no matter how many names it has; the walk counts it once per name, because a directory entry cannot tell that two names share the same extents. So the walk reports *more* for trees like `C:\Windows`.
- **Reparse points.** Zero bytes to the walk; whatever its own record says to the MFT.
- **Permissions.** The walk skips what it cannot read; an elevated MFT scan sees everything.

You can quantify this on your own machine — it is the tool's main correctness test:

```bash
cargo run --release --example crosscheck -- "C:\some\subtree"
```

On the development machine, comparing a whole-volume MFT scan against a walk of a 244,199-file subtree: **8,680 directories present in both, 8,680 byte-for-byte identical, 100.00% agreement**, and no orphaned records. That subtree contains no hard links, which is why it matches exactly; expect a delta on trees like `C:\Windows` that do.

## Troubleshooting

**"Access denied" / large unreadable count.** Expected as a normal user, particularly under `C:\Windows` and other users' profiles. Run from an elevated terminal to see more.

**Paths longer than 260 characters.** Handled; Rust's standard library uses the `\\?\` form internally.

**Totals differ from Explorer's properties dialog.** See [Understanding the numbers](#understanding-the-numbers) — most often compression, sparse files, cluster rounding, or hard links.

**An item will not go to the Recycle Bin.** Windows refuses to recycle items larger than the bin's quota and aborts the operation. `disktree` reports this and suggests `D` (permanent delete) instead.

## Contributing

Issues and pull requests are welcome. Before opening a PR:

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test --all-targets && cargo test --doc
```

Bug reports are much easier to act on with: the `disktree` version, your Windows build, the filesystem, whether you ran elevated, and the exact path pattern involved.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
