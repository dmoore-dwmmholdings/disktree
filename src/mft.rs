//! NTFS Master File Table fast scan (Windows, NTFS, requires elevation).
//!
//! Walking directories costs one metadata read per directory. Reading the MFT
//! instead costs one large sequential pass over a single file, which is why this
//! is dramatically faster on a whole volume. See `docs/mft-fast-path.md` for the
//! measurements and the reasoning behind each decision here.
//!
//! The module is split so that everything difficult is also testable without a
//! volume or privileges: [`apply_fixups`], [`parse_record`], and [`build_tree`]
//! are pure functions over byte slices, and the unit tests drive them with
//! synthetic records covering each way a real volume can produce a wrong answer.

mod io;

pub use io::{Capability, DEFAULT_READERS, Volume, calibrate_readers, probe, scan};

use std::ffi::OsString;

/// Bytes per MFT record on every NTFS volume in practice, and the value used to
/// size synthetic test records. The real stride always comes from
/// `BytesPerFileRecordSegment`, never from this.
#[cfg(test)]
const TEST_RECORD_SIZE: usize = 1024;

/// Record number of the volume root directory. Fixed by the NTFS format.
pub const ROOT_RECORD: u32 = 5;

/// The first 16 records are volume metadata ($MFT, $MFTMirr, $LogFile, …).
/// They are real files and hold real space, but listing them would be noise.
pub const FIRST_ORDINARY_RECORD: u32 = 16;

const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

const FLAG_IN_USE: u16 = 0x0001;
const FLAG_DIRECTORY: u16 = 0x0002;

/// Filename namespaces from `$FILE_NAME`.
const NAME_POSIX: u8 = 0;
const NAME_WIN32: u8 = 1;
const NAME_DOS: u8 = 2;
const NAME_WIN32_AND_DOS: u8 = 3;

/// Why a record produced no usable entry.
#[derive(Debug, PartialEq, Eq)]
pub enum RecordError {
    /// Not a record at all: the slot is free or has never been used.
    NotARecord,
    /// NTFS itself marked the record corrupt.
    Corrupt,
    /// A sector tail did not match the update sequence number. On a live volume
    /// this means the record was rewritten while being read.
    TornRead,
    /// The record is structurally malformed. Possible on an untrusted volume.
    Malformed,
    /// A valid record that is not in use, so it describes a deleted file.
    Deleted,
}

/// One parsed MFT record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// This record's own index.
    pub number: u32,
    /// Record holding this file's base attributes, or `None` if this *is* the base.
    ///
    /// A file whose attributes outgrow one record spills into extension records.
    /// Their sizes must be added back into the base or heavily fragmented files
    /// — exactly the large ones worth finding — are sized zero.
    pub base: Option<u32>,
    /// Best filename, with DOS 8.3 aliases discarded.
    pub name: Option<OsString>,
    /// Record number of the containing directory.
    pub parent: Option<u32>,
    /// Sequence number of the parent, used to detect a recycled directory record.
    pub parent_seq: u16,
    /// Apparent size of the unnamed `$DATA` stream.
    pub size: u64,
    /// Whether this record describes a directory.
    pub is_dir: bool,
    /// Extra `$FILE_NAME` attributes beyond the first non-DOS one, i.e. hard links.
    pub extra_links: u16,
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    b.get(off..off + 2)
        .map(|s| u16::from_le_bytes(s.try_into().unwrap()))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

/// Undo NTFS's update sequence array in place.
///
/// NTFS overwrites the last two bytes of every sector of a record with a
/// sequence number, keeping the real values in an array in the record header, so
/// that a torn write is detectable. Those positions (0x1FE and 0x3FE for a
/// 1024-byte record over 512-byte sectors) fall inside the `$DATA` run list
/// precisely when a file is fragmented, so parsing before restoring them yields
/// sizes that look plausible and are wrong.
///
/// A tail that does not match the sequence number means the record changed
/// underneath the read; that is [`RecordError::TornRead`], not corruption.
pub fn apply_fixups(rec: &mut [u8], bytes_per_sector: usize) -> Result<(), RecordError> {
    match rec.get(0..4) {
        Some(b"FILE") => {}
        Some(b"BAAD") => return Err(RecordError::Corrupt),
        // An unused slot is usually zeroed. That is expected, not an error worth
        // reporting to the user.
        Some([0, 0, 0, 0]) => return Err(RecordError::NotARecord),
        _ => return Err(RecordError::NotARecord),
    }

    let usa_ofs = u16_at(rec, 4).ok_or(RecordError::Malformed)? as usize;
    let usa_cnt = u16_at(rec, 6).ok_or(RecordError::Malformed)? as usize;
    // The count includes the sequence number word itself.
    let n_fixups = usa_cnt.checked_sub(1).ok_or(RecordError::Malformed)?;
    if n_fixups == 0 {
        return Ok(());
    }
    if usa_ofs < 0x30 || usa_ofs + 2 * usa_cnt > rec.len() {
        return Err(RecordError::Malformed);
    }

    let usn = u16_at(rec, usa_ofs).ok_or(RecordError::Malformed)?;
    // Trust the volume's sector size when it tiles the record exactly; otherwise
    // derive the stride, which keeps 4096-byte-sector volumes working.
    let stride = if n_fixups * bytes_per_sector == rec.len() {
        bytes_per_sector
    } else {
        rec.len() / n_fixups
    };
    if stride < 4 {
        return Err(RecordError::Malformed);
    }

    for i in 0..n_fixups {
        let tail = (i + 1) * stride - 2;
        if tail + 2 > rec.len() {
            return Err(RecordError::Malformed);
        }
        if u16_at(rec, tail) != Some(usn) {
            return Err(RecordError::TornRead);
        }
        let real = u16_at(rec, usa_ofs + 2 * (i + 1)).ok_or(RecordError::Malformed)?;
        rec[tail..tail + 2].copy_from_slice(&real.to_le_bytes());
    }
    Ok(())
}

/// Iterate a record's attributes, stopping at the first sign of trouble.
///
/// These are on-disk bytes and may come from an untrusted volume, so every step
/// is bounds-checked and required to move forward.
struct Attributes<'a> {
    rec: &'a [u8],
    off: usize,
    limit: usize,
}

impl<'a> Attributes<'a> {
    fn new(rec: &'a [u8]) -> Result<Self, RecordError> {
        let off = u16_at(rec, 0x14).ok_or(RecordError::Malformed)? as usize;
        let used = u32_at(rec, 0x18).ok_or(RecordError::Malformed)? as usize;
        if off < 0x30 || off > rec.len() {
            return Err(RecordError::Malformed);
        }
        Ok(Attributes {
            rec,
            off,
            limit: used.min(rec.len()),
        })
    }
}

/// A single attribute: its type and its bytes.
struct Attr<'a> {
    atype: u32,
    body: &'a [u8],
}

impl<'a> Iterator for Attributes<'a> {
    type Item = Attr<'a>;

    fn next(&mut self) -> Option<Attr<'a>> {
        if self.off + 8 > self.limit {
            return None;
        }
        let atype = u32_at(self.rec, self.off)?;
        if atype == ATTR_END {
            return None;
        }
        let len = u32_at(self.rec, self.off + 4)? as usize;
        // Zero or unaligned lengths would loop forever or read misaligned.
        if len < 8 || !len.is_multiple_of(8) || self.off + len > self.limit {
            return None;
        }
        let body = self.rec.get(self.off..self.off + len)?;
        self.off += len;
        Some(Attr { atype, body })
    }
}

/// Apparent size of an unnamed `$DATA` attribute, or `None` if it is a named
/// stream or contributes nothing.
fn data_size(a: &Attr<'_>) -> Option<u64> {
    let name_len = *a.body.get(0x09)?;
    // A named $DATA is an alternate data stream. Excluding it matches what
    // `std::fs::Metadata::len` reports, so both scanners agree.
    if name_len != 0 {
        return None;
    }
    let non_resident = *a.body.get(0x08)?;
    if non_resident == 0 {
        // Small files live inside the record. Reading the non-resident size
        // field here would report every one of them as zero bytes, and they are
        // most files by count on a real volume.
        return u32_at(a.body, 0x10).map(u64::from);
    }
    // Only the first extent carries meaningful sizes; continuation extents
    // (lowest_vcn != 0) have junk in those fields by design.
    let lowest_vcn = u64_at(a.body, 0x10)?;
    if lowest_vcn != 0 {
        return None;
    }
    u64_at(a.body, 0x30)
}

/// Decode a `$FILE_NAME` attribute into (parent record, parent sequence, name,
/// namespace).
fn file_name(a: &Attr<'_>) -> Option<(u32, u16, OsString, u8)> {
    let voff = u16_at(a.body, 0x14)? as usize;
    let v = a.body.get(voff..)?;
    let reference = u64_at(v, 0x00)?;
    // A file reference packs a 48-bit record number and a 16-bit sequence number.
    let parent = (reference & 0x0000_FFFF_FFFF_FFFF) as u32;
    let parent_seq = (reference >> 48) as u16;
    let n_chars = *v.get(0x40)? as usize;
    let namespace = *v.get(0x41)?;
    let bytes = v.get(0x42..0x42 + n_chars * 2)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some((
        parent,
        parent_seq,
        OsString::from(String::from_utf16_lossy(&units)),
        namespace,
    ))
}

/// Parse one fixed-up MFT record.
///
/// `rec` must already have had [`apply_fixups`] applied.
pub fn parse_record(rec: &[u8], expected_number: u32) -> Result<Record, RecordError> {
    if rec.len() < 0x30 || rec.get(0..4) != Some(b"FILE") {
        return Err(RecordError::NotARecord);
    }
    let flags = u16_at(rec, 0x16).ok_or(RecordError::Malformed)?;
    let self_number = u32_at(rec, 0x2C).ok_or(RecordError::Malformed)?;
    // A slot that disagrees about its own identity means the read landed in the
    // wrong place; trusting it would attach files to arbitrary parents.
    if self_number != expected_number {
        return Err(RecordError::Malformed);
    }
    if flags & FLAG_IN_USE == 0 {
        return Err(RecordError::Deleted);
    }

    let base_ref = u64_at(rec, 0x20).ok_or(RecordError::Malformed)?;
    let base = match (base_ref & 0x0000_FFFF_FFFF_FFFF) as u32 {
        0 => None,
        n => Some(n),
    };

    let mut best: Option<(OsString, u8)> = None;
    let mut parent = None;
    let mut parent_seq = 0;
    let mut size = 0u64;
    let mut names_seen = 0u16;
    let mut has_attribute_list = false;

    for a in Attributes::new(rec)? {
        match a.atype {
            ATTR_FILE_NAME => {
                let Some((p, seq, name, ns)) = file_name(&a) else {
                    continue;
                };
                // A DOS 8.3 alias names the same file as its Win32 counterpart.
                // Counting both would double the file, and on this machine 94.8%
                // of entries in one tree carried a distinct short name.
                if ns == NAME_DOS {
                    continue;
                }
                names_seen += 1;
                let better = match &best {
                    None => true,
                    // Prefer a real Win32 name over a POSIX one.
                    Some((_, cur)) => {
                        matches!(ns, NAME_WIN32 | NAME_WIN32_AND_DOS) && *cur == NAME_POSIX
                    }
                };
                if better {
                    best = Some((name, ns));
                    parent = Some(p);
                    parent_seq = seq;
                }
            }
            ATTR_DATA => {
                if let Some(s) = data_size(&a) {
                    size = size.max(s);
                }
            }
            ATTR_ATTRIBUTE_LIST => has_attribute_list = true,
            _ => {}
        }
    }

    let is_dir = flags & FLAG_DIRECTORY != 0;
    // A directory has no $DATA; a base record whose $DATA moved into an
    // extension reports zero here and is corrected by the merge pass.
    let _ = has_attribute_list;

    Ok(Record {
        number: expected_number,
        base,
        name: best.map(|(n, _)| n),
        parent,
        parent_seq,
        size: if is_dir { 0 } else { size },
        is_dir,
        extra_links: names_seen.saturating_sub(1),
    })
}

/// Fold extension-record sizes into their base records.
///
/// Returns how many merges happened. A file whose `$DATA` moved wholly into an
/// `$ATTRIBUTE_LIST` extension has `lowest_vcn == 0` *in the extension*, so
/// ignoring extensions sizes it zero — and those are the heavily fragmented
/// large files a disk usage tool exists to find. A real volume must report a
/// nonzero count here, otherwise the path is untested.
pub fn merge_extension_records(records: &mut [Option<Record>]) -> usize {
    let mut merged = 0;
    // Collect first so the borrow of `records` ends before we mutate bases.
    let spill: Vec<(u32, u64)> = records
        .iter()
        .filter_map(|r| r.as_ref())
        .filter_map(|r| match r.base {
            Some(b) if r.size > 0 => Some((b, r.size)),
            _ => None,
        })
        .collect();
    for (base, size) in spill {
        if let Some(Some(target)) = records.get_mut(base as usize) {
            target.size += size;
            merged += 1;
        }
    }
    // Extension records are not files in their own right.
    for r in records.iter_mut() {
        if r.as_ref().is_some_and(|r| r.base.is_some()) {
            *r = None;
        }
    }
    merged
}

/// A node emitted by [`build_tree`], in breadth-first order.
#[derive(Debug, PartialEq, Eq)]
pub struct TreeNode {
    /// MFT record this came from.
    pub record: u32,
    /// Index of the parent in the emitted vector. The root points at itself.
    pub parent: usize,
    /// Name of this entry.
    pub name: OsString,
    /// Apparent size of the file, or 0 for a directory.
    pub size: u64,
    /// Whether this entry is a directory.
    pub is_dir: bool,
}

/// Link parsed records into a tree rooted at the volume root, breadth first.
///
/// Breadth-first emission gives two properties at once: a parent always precedes
/// its children, so subtree sizes aggregate in one reverse pass, and siblings
/// land contiguously, which is what the browser needs.
///
/// Records whose parent is missing, not in use, or has a mismatched sequence
/// number are attached to a synthetic orphan directory rather than to whatever
/// now occupies the recycled parent slot.
pub fn build_tree(records: &[Option<Record>], sequences: &[u16]) -> Vec<TreeNode> {
    let mut children: Vec<Vec<u32>> = vec![Vec::new(); records.len()];
    let mut orphans: Vec<u32> = Vec::new();

    for r in records.iter().flatten() {
        if r.number == ROOT_RECORD || r.number < FIRST_ORDINARY_RECORD {
            continue;
        }
        if r.name.is_none() {
            continue;
        }
        let ok = match r.parent {
            // The root's parent is itself; anything else claiming the root as a
            // parent is fine, but a self-referential non-root record is not.
            Some(p) if (p as usize) < records.len() && p != r.number => {
                let parent_live = records
                    .get(p as usize)
                    .is_some_and(|slot| slot.as_ref().is_some_and(|pr| pr.is_dir));
                let seq_ok = sequences
                    .get(p as usize)
                    .is_none_or(|&s| s == 0 || s == r.parent_seq);
                parent_live && seq_ok
            }
            _ => false,
        };
        if ok {
            children[r.parent.unwrap() as usize].push(r.number);
        } else {
            orphans.push(r.number);
        }
    }

    let mut out: Vec<TreeNode> = Vec::new();
    let root_name = records
        .get(ROOT_RECORD as usize)
        .and_then(|r| r.as_ref())
        .and_then(|r| r.name.clone())
        .unwrap_or_else(|| OsString::from("."));
    out.push(TreeNode {
        record: ROOT_RECORD,
        parent: 0,
        name: root_name,
        size: 0,
        is_dir: true,
    });

    // (record, index of its parent in `out`)
    let mut queue: std::collections::VecDeque<(u32, usize)> = children[ROOT_RECORD as usize]
        .iter()
        .map(|&c| (c, 0))
        .collect();

    while let Some((rec_no, parent_idx)) = queue.pop_front() {
        let Some(Some(r)) = records.get(rec_no as usize) else {
            continue;
        };
        let Some(name) = r.name.clone() else { continue };
        let idx = out.len();
        out.push(TreeNode {
            record: rec_no,
            parent: parent_idx,
            name,
            size: r.size,
            is_dir: r.is_dir,
        });
        if r.is_dir {
            for &c in &children[rec_no as usize] {
                queue.push_back((c, idx));
            }
        }
    }

    if !orphans.is_empty() {
        let orphan_idx = out.len();
        out.push(TreeNode {
            record: u32::MAX,
            parent: 0,
            name: OsString::from("$Orphaned"),
            size: 0,
            is_dir: true,
        });
        let mut queue: std::collections::VecDeque<(u32, usize)> =
            orphans.iter().map(|&c| (c, orphan_idx)).collect();
        while let Some((rec_no, parent_idx)) = queue.pop_front() {
            let Some(Some(r)) = records.get(rec_no as usize) else {
                continue;
            };
            let Some(name) = r.name.clone() else { continue };
            let idx = out.len();
            out.push(TreeNode {
                record: rec_no,
                parent: parent_idx,
                name,
                size: r.size,
                is_dir: r.is_dir,
            });
            if r.is_dir {
                for &c in &children[rec_no as usize] {
                    queue.push_back((c, idx));
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builder for a synthetic MFT record, so each failure mode can be produced
    /// deliberately rather than hoped for on a real volume.
    struct RecordBuilder {
        buf: Vec<u8>,
        attrs: Vec<u8>,
    }

    impl RecordBuilder {
        fn new(number: u32, flags: u16) -> Self {
            let mut buf = vec![0u8; TEST_RECORD_SIZE];
            buf[0..4].copy_from_slice(b"FILE");
            // Update sequence array at 0x30, one USN plus two fixups.
            buf[4..6].copy_from_slice(&0x30u16.to_le_bytes());
            buf[6..8].copy_from_slice(&3u16.to_le_bytes());
            buf[0x10..0x12].copy_from_slice(&1u16.to_le_bytes()); // sequence
            buf[0x12..0x14].copy_from_slice(&1u16.to_le_bytes()); // hard links
            buf[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes()); // attrs offset
            buf[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
            buf[0x2C..0x30].copy_from_slice(&number.to_le_bytes());
            RecordBuilder {
                buf,
                attrs: Vec::new(),
            }
        }

        fn base(mut self, base: u32) -> Self {
            let reference = base as u64 | (1u64 << 48);
            self.buf[0x20..0x28].copy_from_slice(&reference.to_le_bytes());
            self
        }

        fn file_name(mut self, parent: u32, name: &str, namespace: u8) -> Self {
            let units: Vec<u16> = name.encode_utf16().collect();
            let mut value = vec![0u8; 0x42 + units.len() * 2];
            let reference = parent as u64 | (1u64 << 48);
            value[0x00..0x08].copy_from_slice(&reference.to_le_bytes());
            value[0x40] = units.len() as u8;
            value[0x41] = namespace;
            for (i, u) in units.iter().enumerate() {
                value[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
            }
            self.push_attr(ATTR_FILE_NAME, 0, 0, &value);
            self
        }

        /// Resident unnamed `$DATA` of `len` bytes.
        fn resident_data(mut self, len: u32) -> Self {
            let mut header = [0u8; 8];
            header[0..4].copy_from_slice(&len.to_le_bytes());
            self.push_attr(ATTR_DATA, 0, 0, &header[..]);
            // Overwrite the resident value_length field with the real length.
            let at = self.attr_start(ATTR_DATA).unwrap();
            self.attrs[at + 0x10..at + 0x14].copy_from_slice(&len.to_le_bytes());
            self
        }

        /// Non-resident unnamed `$DATA` with the given real size and lowest VCN.
        fn nonresident_data(mut self, real_size: u64, lowest_vcn: u64) -> Self {
            let mut body = vec![0u8; 0x40];
            body[0x00..0x08].copy_from_slice(&lowest_vcn.to_le_bytes());
            body[0x20..0x28].copy_from_slice(&real_size.to_le_bytes()); // allocated
            body[0x28..0x30].copy_from_slice(&real_size.to_le_bytes()); // real
            self.push_attr(ATTR_DATA, 1, 0, &body);
            self
        }

        /// A named `$DATA`, i.e. an alternate data stream.
        fn named_stream(mut self, real_size: u64) -> Self {
            let mut body = vec![0u8; 0x40];
            body[0x20..0x28].copy_from_slice(&real_size.to_le_bytes());
            body[0x28..0x30].copy_from_slice(&real_size.to_le_bytes());
            self.push_attr(ATTR_DATA, 1, 4, &body);
            self
        }

        fn attribute_list(mut self) -> Self {
            self.push_attr(ATTR_ATTRIBUTE_LIST, 0, 0, &[0u8; 8]);
            self
        }

        fn attr_start(&self, atype: u32) -> Option<usize> {
            let mut off = 0;
            while off + 8 <= self.attrs.len() {
                let t = u32::from_le_bytes(self.attrs[off..off + 4].try_into().unwrap());
                let l =
                    u32::from_le_bytes(self.attrs[off + 4..off + 8].try_into().unwrap()) as usize;
                if t == atype {
                    return Some(off);
                }
                if l == 0 {
                    return None;
                }
                off += l;
            }
            None
        }

        fn push_attr(&mut self, atype: u32, non_resident: u8, name_len: u8, value: &[u8]) {
            // header is 0x18 for resident, 0x40 covered by the caller's body for
            // non-resident; keep it simple and uniform at 0x18 + value.
            let header_len = if non_resident == 0 { 0x18 } else { 0x10 };
            let total = (header_len + value.len()).next_multiple_of(8);
            let mut a = vec![0u8; total];
            a[0..4].copy_from_slice(&atype.to_le_bytes());
            a[4..8].copy_from_slice(&(total as u32).to_le_bytes());
            a[0x08] = non_resident;
            a[0x09] = name_len;
            if non_resident == 0 {
                a[0x14..0x16].copy_from_slice(&(header_len as u16).to_le_bytes());
            }
            a[header_len..header_len + value.len()].copy_from_slice(value);
            self.attrs.extend_from_slice(&a);
        }

        /// Finish the record, writing attributes and a valid update sequence array.
        fn build(mut self) -> Vec<u8> {
            let start = 0x38usize;
            let used = start + self.attrs.len() + 8;
            self.buf[start..start + self.attrs.len()].copy_from_slice(&self.attrs);
            let end = start + self.attrs.len();
            self.buf[end..end + 4].copy_from_slice(&ATTR_END.to_le_bytes());
            self.buf[0x18..0x1C].copy_from_slice(&(used as u32).to_le_bytes());
            self.buf[0x1C..0x20].copy_from_slice(&(TEST_RECORD_SIZE as u32).to_le_bytes());

            // Stamp the USN into both sector tails and record the real bytes.
            let usn: u16 = 0xBEEF;
            self.buf[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
            for i in 0..2usize {
                let tail = (i + 1) * 512 - 2;
                let real = u16::from_le_bytes(self.buf[tail..tail + 2].try_into().unwrap());
                self.buf[0x32 + i * 2..0x34 + i * 2].copy_from_slice(&real.to_le_bytes());
                self.buf[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
            }
            self.buf
        }
    }

    fn fixed_up(mut rec: Vec<u8>) -> Vec<u8> {
        apply_fixups(&mut rec, 512).expect("fixups apply");
        rec
    }

    #[test]
    fn fixups_restore_sector_tails() {
        // Put a recognisable value where a sector tail lands.
        let mut b = RecordBuilder::new(42, FLAG_IN_USE);
        b.buf[510..512].copy_from_slice(&0x1234u16.to_le_bytes());
        b.buf[1022..1024].copy_from_slice(&0x5678u16.to_le_bytes());
        let rec = b.build();
        // Before fixups the tails hold the USN, not the data.
        assert_eq!(
            u16::from_le_bytes(rec[510..512].try_into().unwrap()),
            0xBEEF
        );
        let rec = fixed_up(rec);
        assert_eq!(
            u16::from_le_bytes(rec[510..512].try_into().unwrap()),
            0x1234
        );
        assert_eq!(
            u16::from_le_bytes(rec[1022..1024].try_into().unwrap()),
            0x5678
        );
    }

    #[test]
    fn fixups_reject_torn_record() {
        let mut rec = RecordBuilder::new(42, FLAG_IN_USE).build();
        // Simulate a sector rewritten mid-read.
        rec[510..512].copy_from_slice(&0x0000u16.to_le_bytes());
        assert_eq!(apply_fixups(&mut rec, 512), Err(RecordError::TornRead));
    }

    #[test]
    fn fixups_reject_baad_and_empty() {
        let mut baad = RecordBuilder::new(1, FLAG_IN_USE).build();
        baad[0..4].copy_from_slice(b"BAAD");
        assert_eq!(apply_fixups(&mut baad, 512), Err(RecordError::Corrupt));

        let mut zero = vec![0u8; TEST_RECORD_SIZE];
        assert_eq!(apply_fixups(&mut zero, 512), Err(RecordError::NotARecord));
    }

    /// Most files by count are small enough to live inside their MFT record.
    /// Reading the non-resident size field for them would report zero bytes.
    #[test]
    fn resident_data_is_sized_from_the_value_length() {
        let rec = fixed_up(
            RecordBuilder::new(64, FLAG_IN_USE)
                .file_name(ROOT_RECORD, "small.txt", NAME_WIN32)
                .resident_data(300)
                .build(),
        );
        let r = parse_record(&rec, 64).expect("parses");
        assert_eq!(r.size, 300, "resident $DATA sized wrongly");
        assert_eq!(r.name.as_deref(), Some(std::ffi::OsStr::new("small.txt")));
        assert!(!r.is_dir);
    }

    #[test]
    fn nonresident_first_extent_is_sized_from_real_size() {
        let rec = fixed_up(
            RecordBuilder::new(65, FLAG_IN_USE)
                .file_name(ROOT_RECORD, "big.bin", NAME_WIN32)
                .nonresident_data(4 * 1024 * 1024 * 1024, 0)
                .build(),
        );
        let r = parse_record(&rec, 65).expect("parses");
        assert_eq!(r.size, 4 * 1024 * 1024 * 1024);
    }

    /// Continuation extents carry meaningless size fields by design.
    #[test]
    fn continuation_extent_contributes_nothing() {
        let rec = fixed_up(
            RecordBuilder::new(66, FLAG_IN_USE)
                .file_name(ROOT_RECORD, "frag.bin", NAME_WIN32)
                .nonresident_data(0xDEAD_BEEF, 1)
                .build(),
        );
        let r = parse_record(&rec, 66).expect("parses");
        assert_eq!(r.size, 0, "junk size from a continuation extent was used");
    }

    /// An alternate data stream must not inflate the file's size, so that the
    /// MFT scan agrees with `metadata().len()`.
    #[test]
    fn named_stream_is_excluded() {
        let rec = fixed_up(
            RecordBuilder::new(67, FLAG_IN_USE)
                .file_name(ROOT_RECORD, "host.txt", NAME_WIN32)
                .resident_data(100)
                .named_stream(999_999)
                .build(),
        );
        let r = parse_record(&rec, 67).expect("parses");
        assert_eq!(r.size, 100, "an alternate data stream was counted");
    }

    /// A file with an 8.3 alias has two $FILE_NAME attributes for one file.
    #[test]
    fn dos_short_name_does_not_double_count() {
        let rec = fixed_up(
            RecordBuilder::new(68, FLAG_IN_USE)
                .file_name(ROOT_RECORD, "LongName.txt", NAME_WIN32)
                .file_name(ROOT_RECORD, "LONGNA~1.TXT", NAME_DOS)
                .resident_data(10)
                .build(),
        );
        let r = parse_record(&rec, 68).expect("parses");
        assert_eq!(
            r.name.as_deref(),
            Some(std::ffi::OsStr::new("LongName.txt"))
        );
        assert_eq!(r.extra_links, 0, "the 8.3 alias was counted as a hard link");
    }

    #[test]
    fn hard_links_are_reported() {
        let rec = fixed_up(
            RecordBuilder::new(69, FLAG_IN_USE)
                .file_name(ROOT_RECORD, "one.txt", NAME_WIN32)
                .file_name(7, "two.txt", NAME_WIN32)
                .resident_data(10)
                .build(),
        );
        let r = parse_record(&rec, 69).expect("parses");
        assert_eq!(r.extra_links, 1);
    }

    #[test]
    fn deleted_and_misplaced_records_are_rejected() {
        let rec = fixed_up(RecordBuilder::new(70, 0).build());
        assert_eq!(parse_record(&rec, 70), Err(RecordError::Deleted));

        let rec = fixed_up(RecordBuilder::new(70, FLAG_IN_USE).build());
        // Reading the slot as if it were a different record must be refused.
        assert_eq!(parse_record(&rec, 71), Err(RecordError::Malformed));
    }

    /// The case that silently zeroes the largest fragmented files.
    #[test]
    fn attribute_list_spillover_is_merged_into_the_base() {
        let base = fixed_up(
            RecordBuilder::new(80, FLAG_IN_USE)
                .file_name(ROOT_RECORD, "huge.vhdx", NAME_WIN32)
                .attribute_list()
                .build(),
        );
        let ext = fixed_up(
            RecordBuilder::new(81, FLAG_IN_USE)
                .base(80)
                .nonresident_data(4 * 1024 * 1024 * 1024, 0)
                .build(),
        );

        let mut records: Vec<Option<Record>> = vec![None; 82];
        records[80] = Some(parse_record(&base, 80).unwrap());
        records[81] = Some(parse_record(&ext, 81).unwrap());
        assert_eq!(records[80].as_ref().unwrap().size, 0, "base starts at 0");

        let merged = merge_extension_records(&mut records);
        assert_eq!(merged, 1);
        assert_eq!(
            records[80].as_ref().unwrap().size,
            4 * 1024 * 1024 * 1024,
            "spillover size was lost"
        );
        assert!(records[81].is_none(), "extension record became a file");
    }

    #[test]
    fn malformed_attribute_chain_terminates() {
        let mut rec = fixed_up(
            RecordBuilder::new(90, FLAG_IN_USE)
                .file_name(ROOT_RECORD, "x", NAME_WIN32)
                .build(),
        );
        // A zero attribute length would spin forever if not rejected.
        rec[0x38 + 4..0x38 + 8].copy_from_slice(&0u32.to_le_bytes());
        let r = parse_record(&rec, 90).expect("still parses the header");
        assert!(r.name.is_none(), "attributes past the break were trusted");
    }

    /// Random bytes must never panic or hang, since a volume can be untrusted.
    #[test]
    fn random_bytes_never_panic() {
        let mut state = 0x12345678u64;
        for _ in 0..2000 {
            let mut rec = vec![0u8; TEST_RECORD_SIZE];
            for b in rec.iter_mut() {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                *b = (state >> 33) as u8;
            }
            rec[0..4].copy_from_slice(b"FILE");
            let _ = apply_fixups(&mut rec, 512);
            let _ = parse_record(
                &rec,
                u32::from_le_bytes(rec[0x2C..0x30].try_into().unwrap()),
            );
        }
    }

    fn dir(number: u32, parent: u32, name: &str) -> Option<Record> {
        Some(Record {
            number,
            base: None,
            name: Some(OsString::from(name)),
            parent: Some(parent),
            parent_seq: 1,
            size: 0,
            is_dir: true,
            extra_links: 0,
        })
    }

    fn file(number: u32, parent: u32, name: &str, size: u64) -> Option<Record> {
        Some(Record {
            number,
            base: None,
            name: Some(OsString::from(name)),
            parent: Some(parent),
            parent_seq: 1,
            size,
            is_dir: false,
            extra_links: 0,
        })
    }

    #[test]
    fn tree_is_breadth_first_with_parents_before_children() {
        let mut records: Vec<Option<Record>> = vec![None; 40];
        records[ROOT_RECORD as usize] = dir(ROOT_RECORD, ROOT_RECORD, "C:");
        records[20] = dir(20, ROOT_RECORD, "sub");
        records[21] = file(21, 20, "inner.txt", 500);
        records[22] = file(22, ROOT_RECORD, "top.txt", 100);
        let sequences = vec![1u16; 40];

        let tree = build_tree(&records, &sequences);
        assert_eq!(tree[0].record, ROOT_RECORD);
        for (i, n) in tree.iter().enumerate().skip(1) {
            assert!(
                n.parent < i,
                "node {i} has parent {} not before it",
                n.parent
            );
        }
        let names: Vec<String> = tree
            .iter()
            .map(|n| n.name.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"sub".to_owned()));
        assert!(names.contains(&"inner.txt".to_owned()));
        assert!(names.contains(&"top.txt".to_owned()));
    }

    /// The root's parent is itself; a naive walk would loop forever.
    #[test]
    fn root_self_cycle_terminates() {
        let mut records: Vec<Option<Record>> = vec![None; 40];
        records[ROOT_RECORD as usize] = dir(ROOT_RECORD, ROOT_RECORD, "C:");
        let tree = build_tree(&records, &[1u16; 40]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].parent, 0);
    }

    /// A parent whose sequence number moved on has been recycled into a
    /// different directory; attaching there would misreport the file's location.
    #[test]
    fn stale_parent_reference_becomes_orphan() {
        let mut records: Vec<Option<Record>> = vec![None; 40];
        records[ROOT_RECORD as usize] = dir(ROOT_RECORD, ROOT_RECORD, "C:");
        records[20] = dir(20, ROOT_RECORD, "live");
        let mut stale = file(30, 20, "lost.txt", 10).unwrap();
        stale.parent_seq = 99;
        records[30] = Some(stale);

        let mut sequences = vec![1u16; 40];
        sequences[20] = 7; // parent slot was reused
        let tree = build_tree(&records, &sequences);

        let orphan_root = tree
            .iter()
            .position(|n| n.name == "$Orphaned")
            .expect("orphan container missing");
        let lost = tree
            .iter()
            .find(|n| n.name == "lost.txt")
            .expect("orphaned file dropped entirely");
        assert_eq!(lost.parent, orphan_root);
    }

    #[test]
    fn missing_parent_becomes_orphan() {
        let mut records: Vec<Option<Record>> = vec![None; 40];
        records[ROOT_RECORD as usize] = dir(ROOT_RECORD, ROOT_RECORD, "C:");
        records[31] = file(31, 25, "nowhere.txt", 10); // record 25 does not exist
        let tree = build_tree(&records, &[1u16; 40]);
        assert!(
            tree.iter().any(|n| n.name == "nowhere.txt"),
            "a file with a missing parent was silently dropped"
        );
    }
}
