# Design note: NTFS MFT fast scan

Status: **implemented** in `src/mft.rs` (parsing, unit-tested) and
`src/mft/io.rs` (volume access). This records why each decision was made, and
what was measured rather than assumed.

Milestone zero passed on the development machine: `FILE_READ_DATA` on `\\.\C:`
succeeds when elevated, `FSCTL_GET_RETRIEVAL_POINTERS` mapped `$MFT` across **35
fragments** covering `MftValidDataLength` exactly, and record 0 validated its
fixups and self-identified as `$MFT`. That last result is the one that matters:
it proves geometry, extent mapping, aligned raw reads and the fixup logic are
all correct together.

## Why

Scanning is bound on directory-metadata I/O, not CPU. Measured on
`C:\Users\<user>\.rustup` (244,164 files in 8,678 directories), release build:

| Cache state | Time | files/s |
|---|---|---|
| Cold | 5.269 s | 46,340 |
| Warm | 0.068 s | 3,596,698 |

A 75x spread. The CPU path already sustains 3.6M files/s, so no amount of
per-entry optimisation matters; only changing the *access pattern* does. Reading
the Master File Table turns scattered per-directory index reads into one large
sequential read, which is how WizTree scans a volume in seconds.

`C:`'s MFT on the development machine is 6,866,075,648 bytes over 6,705,152
record slots at 1024 bytes each (from `FSCTL_GET_NTFS_VOLUME_DATA`, which needs
no elevation). Compare 41 s cold for a 1.6M-file *subtree* today. Note the cost
scales with MFT **size**, not live file count: roughly half those slots are dead
and interleaved, and nothing can skip them.

Measured, not projected: a full `C:` scan takes **10.3 to 21.1 s** across repeated
runs (327-669 MB/s of `$MFT`), against 41 s for a cold walk of one 1.6M-file
subtree. Two earlier estimates in this file were wrong in both directions — 5-7 s
from assuming ~1.7 GB/s, then 16.7 s from a queue-depth-1 probe — which is why
the figure above comes from the real scan. Note the 2x run-to-run spread on
identical input; quote a range, never a single number.

## Hard constraints, measured not assumed

`examples/probe.rs` reports these for any volume at the current privilege level.
As a standard user on `\\.\C:`:

| Attempt | Result |
|---|---|
| Open with `access=0` | OK |
| Open with `FILE_READ_ATTRIBUTES` | OK |
| Open with `FILE_READ_DATA` | **error 5**, `ERROR_ACCESS_DENIED` |
| Open with `GENERIC_READ` | **error 5** |
| `FSCTL_QUERY_USN_JOURNAL` on a low-privilege handle | error 1 |
| `FSCTL_ENUM_USN_DATA` on a low-privilege handle | error 1 |

So:

1. **The fast path requires Administrator.** There is no unelevated substitute.
   WizTree prompts for UAC for exactly this reason.
2. **A successful `CreateFileW` is not a capability check.** Several access masks
   open the volume unelevated. The gate is `FILE_READ_DATA`.
3. **Never distinguish "not NTFS" by `ERROR_INVALID_FUNCTION`.** Error 1 is
   returned both by a non-NTFS volume and by an ioctl issued on the wrong shape
   of handle, so the two are indistinguishable. Use `GetVolumeInformationW` and
   require the filesystem name to be exactly `NTFS`, failing closed.
4. **The USN journal cannot replace MFT parsing.** `USN_RECORD_V2` is
   `RecordLength, Major/MinorVersion, FileReferenceNumber,
   ParentFileReferenceNumber, Usn, TimeStamp, Reason, SourceInfo, SecurityId,
   FileAttributes, FileNameLength, FileNameOffset`, then the name. There is **no
   size field** in V2, V3 or V4. It can recover tree structure cheaply but cannot
   size anything.

## Get the extent map from an ioctl, not a runlist decoder

The `$MFT` is fragmented, so its own `$DATA` run list must be followed rather
than assuming contiguity. The obvious approach — parse record 0 and decode its
run list — should **not** be built:

- Hand-decoding record 0 has an unbounded bootstrap problem: a sufficiently
  fragmented `$MFT` spills record 0 into `$ATTRIBUTE_LIST` extension records
  stored elsewhere *in the MFT*, which cannot be located until the MFT is
  already mapped.
- `FSCTL_GET_RETRIEVAL_POINTERS` on a handle to `$MFT` returns the same extent
  map directly. It needs **no access rights** (`dwDesiredAccess = 0` suffices),
  and it is how `fsutil file queryextents` and Sysinternals `Contig -a` work.
- `$MFT` cannot be opened by name unelevated — but unelevated you cannot read a
  single MFT byte either, so the decoder would be dead code in the only regime
  that motivated it.

Loop on `ERROR_MORE_DATA` (234), advancing `StartingVcn` to the last returned
`NextVcn`. Even elevated, `ReadFile` on the `$MFT` handle itself is refused; that
handle is for the ioctl only, and the bytes come from the volume device at the
returned LCNs.

## Parsing order that avoids known-wrong results

1. **Apply update-sequence-array fixups before reading anything past 0x30.**
   `usa_ofs@0x04`, `usa_cnt@0x06` (the count includes the USN word). Each
   sector's final two bytes must equal the USN and are then restored from the
   array. Skipping this corrupts offsets 0x1FE and 0x3FE — which fall inside the
   `$DATA` run list precisely when a file is fragmented, yielding
   plausible-but-wrong sizes. A mismatch means a torn read of a live volume:
   count it as an error, do not parse it.
2. `flags@0x16`: bit 0 in use, bit 1 directory. Not in use means no node.
3. `base_ref@0x20` nonzero means this is an extension record — **keep the value**,
   step 6 needs it.
4. Walk attributes from `offset@0x14`, bounded by `used_size@0x18`. Reject zero
   or non-8-aligned lengths and guarantee forward progress: these are on-disk
   bytes, possibly from an untrusted VHD or USB volume.
5. `$FILE_NAME` (0x30): **skip namespace 2 (DOS)** or every file with an 8.3 name
   is counted twice — 94.8% of `.rustup`'s entries carry a distinct short name.
   Namespace 3 means the long name is already 8.3-legal and must be kept.
6. `$DATA` (0x80), **unnamed only** (a named `$DATA` is an alternate stream, and
   excluding it matches `metadata().len()`):
   - **Resident** (`non_resident == 0`): size is the attribute's value length.
     Reading `real_size` from the non-resident header here would size **every
     small file as 0**, and small files are most files by count.
   - **Non-resident with `lowest_vcn@0x10 == 0`**: `real_size@0x30`.
   - Continuation extents (`lowest_vcn != 0`) carry meaningless size fields —
     never read sizes from them.
   - Never take sizes from `$FILE_NAME`'s size fields or from directory index
     entries; those update only when a filename changes and go stale by design.
7. **Merge extension records**: for each record with `base_ref != 0` and a size,
   add it into the base record. A file whose `$DATA` moved wholly into an
   `$ATTRIBUTE_LIST` extension has `lowest_vcn == 0` *in the extension*, so
   ignoring extensions sizes it 0 — exactly the huge fragmented files this tool
   exists to find. Report the merge count; it must be nonzero on a real volume.
8. Root is record 5 and **its parent is itself** — guard that self-cycle.
   Validate each parent's sequence number; on mismatch, or a parent that is
   missing or not in use, attach to a synthetic orphan node rather than to a
   recycled directory. Emit in BFS order so `parent < child`.

   Compare a child's recorded parent sequence against **the parent record's own
   sequence number**, which means every record must carry its own
   (`sequence_number@0x10`). An earlier version kept sequences in a parallel array
   that the caller filled with each record's *parent's* sequence instead, so every
   comparison failed and the whole volume landed in the orphan container — while
   the total size stayed correct, because orphaned files still aggregate. Every
   isolated unit test passed too, since they populated that array by hand and so
   honoured a contract the real caller broke. `build_tree` now reads the sequence
   from the `Record`, deleting the parallel array and that whole class of bug, and
   `parsed_records_link_into_a_real_tree` drives `parse_record` output into
   `build_tree` to cover the seam. Watch the orphan share: it should be a handful
   of files, not a percentage.

Declare `BY_HANDLE_FILE_INFORMATION` and `NTFS_VOLUME_DATA_BUFFER` from
`windows-sys`, never by hand: getting one field's type wrong shifts every later
field by the resulting padding and silently produces absurd values. Take the
record stride from `BytesPerFileRecordSegment`, not
`ClustersPerFileRecordSegment` — the latter is 0 on all five volumes here (a
1024-byte record inside a 4096-byte cluster) and dividing by it faults.

## Order of work

**Milestone zero, before any parser** — done, via `examples/mftprobe.rs`: an
elevated run that opens the volume with `FILE_READ_DATA`, maps `$MFT` via the
ioctl, asserts `sum(extent clusters) * bytes_per_cluster >= MftValidDataLength`,
and reads record 0 to confirm its fixups apply and it identifies as `$MFT`. That
check exists so a failure names one API instead of pointing somewhere inside the
parser; keep it working.

Then: read 16 MiB chunks with `FILE_FLAG_NO_BUFFERING`, which requires offset,
length *and* buffer address all sector-aligned. Stop at `MftValidDataLength`.

**Corrected after measurement: use several concurrent readers, not one.** The
research concluded a single reader thread was ample because parsing (~3.4M
records/s) far outruns a device assumed to do ~1.7 GB/s. But a synchronous
`ReadFile` loop keeps exactly one request outstanding, and the raw-volume probe
measured only **411 MB/s** at that depth, which would make the reader — not the
disk — the bottleneck. Calibrating on the same drive with unbuffered 16 MiB reads
(`examples/iocal.rs`, no elevation needed):

| Concurrent readers | MB/s | relative |
|---|---|---|
| 1 | 1774 | 1.00x |
| 2 | 3321 | 1.87x |
| 4 | 3595 | 2.03x |
| 8 | 3613 | 2.04x |
| 16 | 3390 | 1.91x |
| 32 | 3518 | 1.98x |

The knee is at 4 and it plateaus by 8, which is the default. Each reader opens
its own handle: a shared file pointer would serialise the very seeks being
overlapped.

Confirmed on the real thing. `DISKTREE_MFT_TIMING=1` reports summed reader time
over wall time, and a full scan shows **6.3-7.1x overlap** — the readers really do
run concurrently against the raw volume. Parsing takes only ~2.5 s of a 10-21 s
scan, so the device is the constraint and the parser has ample headroom; there is
no case for overlapped I/O or IOCP.

Treat the absolute figures as device-dependent: a freshly written file read at
1774 MB/s while the raw volume managed 327-669 MB/s, and the MFT region of a
DRAM-less drive is not equivalent to either. The transferable result is that
concurrency is worth roughly 2x and that 8 readers is safely on the plateau.

Gate it: use the fast path only when elevated, the filesystem is NTFS, and the
requested path is the volume root; `--mft` to force, `--no-mft` to disable. Fall
back to the directory walk with a visible one-line notice, never a panic and
never a self-triggered UAC prompt.

## Validation result

The cross-check passed on the development machine: a whole-volume MFT scan
against a walk of a 244,199-file subtree gave **8,680 directories present in
both, 8,680 byte-for-byte identical, 100.00% agreement**, no orphaned records,
and target totals equal to the byte (10.517 GB each). Whole-volume total was
1783.54 GB against 1.82 TB of used space.

That one result exercises fixups, resident and non-resident sizing, extension
merging, DOS-name deduplication, alternate-stream exclusion and parent linking
simultaneously, which is why it is worth more than any single unit test. Keep it
working.

## The test that decides whether it is correct

A cross-check example that scans a real mid-size subtree **both ways** and diffs
per-node sizes and counts, asserting agreement except for an itemised,
whitelisted delta (hard links, alternate streams, reparse points) and printing
the breakdown. Unit tests over synthetic 1024-byte records cover what the
cross-check cannot isolate: resident `$DATA` sizing, extension-record merging,
DOS-name deduplication, continuation-extent junk, torn-record rejection, and a
fuzz loop asserting no panic or hang on random bytes.
