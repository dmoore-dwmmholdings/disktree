# disktree

A disk usage TUI for your terminal, in the spirit of [WizTree](https://diskanalyzer.com/) and ncdu: scan a drive or folder, see what's taking space at a glance, and delete it safely — without leaving the console.

- **Fast parallel scan** with live progress (1.6M files / 762 GB in ~40 s on a typical NVMe drive)
- **Size-sorted browser** with percentage bars, human-readable sizes, and recursive item counts
- **Safe deletion**: mark any number of files or folders, send them to the **Recycle Bin** (recoverable) or permanently delete — always behind a confirmation dialog listing exactly what will go
- Sizes and percentages update instantly after deletion, no rescan needed
- Unreadable (permission-denied) directories are counted and reported, never fatal

## Install

```bash
cargo install disktree
```

## Usage

```bash
disktree                 # scan C:\ (Windows default)
disktree --path D:\Data  # scan a specific directory
```

## Keys

| Key | Action |
|---|---|
| `↑` `↓` / `j` `k` | Move selection |
| `PgUp` `PgDn` / `Home` `End` | Jump |
| `Enter` / `→` / `l` | Open directory |
| `Backspace` / `←` / `h` | Go up |
| `Space` | Mark / unmark item |
| `c` | Clear all marks |
| `d` | Delete marked (or selected) → **Recycle Bin**, with confirmation |
| `D` | Delete **permanently**, with confirmation |
| `r` | Rescan |
| `q` / `Esc` | Quit |

The footer shows how many items are marked and their total size before you commit to anything.

## Notes

- Sizes are apparent file sizes (`metadata.len()`), matching WizTree's default column.
- Symlinks and junctions are skipped to avoid cycles and double counting.
- Scanning walks directories via `read_dir`; it does not parse the NTFS MFT, so it works on any filesystem and without admin rights, at the cost of some speed versus MFT-based tools.
- Recycle Bin deletion uses the [trash](https://crates.io/crates/trash) crate, so it also works on macOS/Linux (system trash), though Windows is the primary target.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
