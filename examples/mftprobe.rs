//! Milestone-zero check for the MFT fast scan: `cargo run --release --example mftprobe -- C`
//!
//! Runs the entire risky sequence end to end with no parser and no scanning, so
//! a failure points at one specific API rather than at 500 lines of code:
//!
//!   1. NTFS geometry via FSCTL_GET_NTFS_VOLUME_DATA (works unelevated)
//!   2. open the raw volume with FILE_READ_DATA          <- the elevation gate
//!   3. map $MFT's extents via FSCTL_GET_RETRIEVAL_POINTERS on an access=0 handle
//!   4. assert the extents actually cover MftValidDataLength
//!   5. read MFT record 0 off the raw device, apply update-sequence fixups, and
//!      confirm it is $MFT itself
//!   6. time a 16 MiB sequential read to size the real scan
//!
//! Prints GO or NO-GO. Must be run from an elevated shell to reach step 2.

use std::alloc::{Layout, alloc, dealloc};
use std::ffi::{OsString, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_BEGIN, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING,
    FILE_FLAG_SEQUENTIAL_SCAN, FILE_READ_DATA, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    ReadFile, SetFilePointerEx,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_GET_RETRIEVAL_POINTERS, NTFS_VOLUME_DATA_BUFFER,
};

const ERROR_MORE_DATA: i32 = 234;

/// A heap buffer whose address is sector-aligned, as FILE_FLAG_NO_BUFFERING requires.
struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
    layout: Layout,
}

impl AlignedBuf {
    fn new(len: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(len, align).expect("valid layout");
        // SAFETY: len is nonzero and align is a power of two from the volume geometry.
        let ptr = unsafe { alloc(layout) };
        assert!(!ptr.is_null(), "allocation of {len} bytes failed");
        AlignedBuf { ptr, len, layout }
    }

    fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr is a live allocation of exactly len bytes.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, and &mut self prevents aliasing.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: ptr came from alloc with this exact layout and is freed once.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

fn wide(s: &str) -> Vec<u16> {
    OsString::from(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn last_err() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

fn open(path: &str, access: u32, flags: u32) -> Result<HANDLE, i32> {
    let w = wide(path);
    // SAFETY: w is NUL-terminated UTF-16; the remaining arguments are scalars.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        Err(last_err())
    } else {
        Ok(h)
    }
}

/// One `$MFT` fragment, in clusters.
#[derive(Debug, Clone, Copy)]
struct Extent {
    lcn: i64,
    clusters: i64,
}

/// Ask NTFS where `$MFT` lives. Needs no access rights on the handle.
fn mft_extents(h: HANDLE) -> Result<Vec<Extent>, i32> {
    let mut out = Vec::new();
    let mut start_vcn: i64 = 0;
    // Room for many extents per call; the MFT is usually a handful of fragments.
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let input = start_vcn;
        let mut returned = 0u32;
        // SAFETY: input is a STARTING_VCN_INPUT_BUFFER (one i64); buf is a plain
        // byte buffer receiving RETRIEVAL_POINTERS_BUFFER, read via byte offsets.
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_GET_RETRIEVAL_POINTERS,
                &input as *const i64 as *const c_void,
                size_of::<i64>() as u32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        let err = if ok == 0 { last_err() } else { 0 };
        if ok == 0 && err != ERROR_MORE_DATA {
            return Err(err);
        }

        // RETRIEVAL_POINTERS_BUFFER: ExtentCount u32, pad, StartingVcn i64,
        // then ExtentCount * { NextVcn i64, Lcn i64 }.
        let count = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let mut vcn = i64::from_le_bytes(buf[8..16].try_into().unwrap());
        for i in 0..count {
            let base = 16 + i * 16;
            if base + 16 > buf.len() {
                break;
            }
            let next_vcn = i64::from_le_bytes(buf[base..base + 8].try_into().unwrap());
            let lcn = i64::from_le_bytes(buf[base + 8..base + 16].try_into().unwrap());
            let clusters = next_vcn - vcn;
            // LCN -1 marks a sparse run, which $MFT should never have.
            if lcn >= 0 && clusters > 0 {
                out.push(Extent { lcn, clusters });
            }
            vcn = next_vcn;
        }
        if ok != 0 {
            return Ok(out);
        }
        if vcn == start_vcn {
            // No forward progress; stop rather than spin.
            return Ok(out);
        }
        start_vcn = vcn;
    }
}

/// Restore a record's sector tails from its update sequence array.
///
/// Every `stride` bytes NTFS overwrites the final two bytes with a sequence
/// number and stashes the real value in the array. Parsing without undoing this
/// corrupts offsets 0x1FE and 0x3FE — which land inside the `$DATA` run list
/// exactly when a file is fragmented.
fn apply_fixups(rec: &mut [u8], bytes_per_sector: usize) -> Result<(), String> {
    if &rec[0..4] == b"BAAD" {
        return Err("record marked BAAD (NTFS flagged it corrupt)".into());
    }
    if &rec[0..4] != b"FILE" {
        return Err(format!("bad magic {:02x?}, expected \"FILE\"", &rec[0..4]));
    }
    let usa_ofs = u16::from_le_bytes(rec[4..6].try_into().unwrap()) as usize;
    let usa_cnt = u16::from_le_bytes(rec[6..8].try_into().unwrap()) as usize;
    if usa_cnt == 0 {
        return Err("update sequence count is 0".into());
    }
    let n_fixups = usa_cnt - 1; // the count includes the USN word itself
    if n_fixups == 0 {
        return Ok(());
    }
    if usa_ofs + 2 * usa_cnt > rec.len() {
        return Err("update sequence array runs past the record".into());
    }

    let usn = u16::from_le_bytes(rec[usa_ofs..usa_ofs + 2].try_into().unwrap());
    // Prefer the volume's sector size; fall back to deriving it from the count.
    let stride = if n_fixups * bytes_per_sector == rec.len() {
        bytes_per_sector
    } else {
        rec.len() / n_fixups
    };

    for i in 0..n_fixups {
        let tail = (i + 1) * stride - 2;
        if tail + 2 > rec.len() {
            return Err(format!("fixup {i} tail at {tail} is outside the record"));
        }
        let found = u16::from_le_bytes(rec[tail..tail + 2].try_into().unwrap());
        if found != usn {
            return Err(format!(
                "fixup {i}: sector tail {found:#06x} != USN {usn:#06x} (torn read or wrong stride {stride})"
            ));
        }
        let src = usa_ofs + 2 * (i + 1);
        let real = u16::from_le_bytes(rec[src..src + 2].try_into().unwrap());
        rec[tail..tail + 2].copy_from_slice(&real.to_le_bytes());
    }
    Ok(())
}

/// The unnamed `$FILE_NAME` of a record, if present. Used only to confirm that
/// record 0 really is `$MFT`, which proves the whole read path lines up.
fn first_file_name(rec: &[u8]) -> Option<String> {
    let attrs_off = u16::from_le_bytes(rec[0x14..0x16].try_into().unwrap()) as usize;
    let used = u32::from_le_bytes(rec[0x18..0x1C].try_into().unwrap()) as usize;
    let limit = used.min(rec.len());
    let mut off = attrs_off;
    while off + 8 <= limit {
        let atype = u32::from_le_bytes(rec[off..off + 4].try_into().unwrap());
        if atype == 0xFFFF_FFFF {
            break;
        }
        let alen = u32::from_le_bytes(rec[off + 4..off + 8].try_into().unwrap()) as usize;
        if alen == 0 || !alen.is_multiple_of(8) || off + alen > limit {
            break;
        }
        if atype == 0x30 {
            let voff = u16::from_le_bytes(rec[off + 0x14..off + 0x16].try_into().unwrap()) as usize;
            let v = off + voff;
            if v + 0x42 <= limit {
                let n_chars = rec[v + 0x40] as usize;
                let start = v + 0x42;
                let end = start + n_chars * 2;
                if end <= limit {
                    let units: Vec<u16> = rec[start..end]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    return Some(String::from_utf16_lossy(&units));
                }
            }
        }
        off += alen;
    }
    None
}

fn main() {
    let letter = std::env::args()
        .nth(1)
        .and_then(|s| s.chars().next())
        .unwrap_or('C')
        .to_ascii_uppercase();
    println!("MFT fast-path milestone-zero probe, volume {letter}:\n");

    // --- 1. geometry (no elevation needed) ---------------------------------
    println!("== 1. NTFS geometry ==");
    let root = format!("{letter}:\\");
    let root_h = match open(&root, 0, FILE_FLAG_BACKUP_SEMANTICS) {
        Ok(h) => h,
        Err(e) => {
            println!("  cannot open {root} (os error {e})");
            println!("\nNO-GO: could not open the volume root.");
            return;
        }
    };
    let mut vd = NTFS_VOLUME_DATA_BUFFER::default();
    let mut returned = 0u32;
    // SAFETY: root_h is a directory handle; vd is exactly the struct this ioctl
    // fills, and its size is passed from size_of.
    let ok = unsafe {
        DeviceIoControl(
            root_h,
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            &mut vd as *mut _ as *mut c_void,
            size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: opened above and closed once.
    unsafe { CloseHandle(root_h) };
    if ok == 0 {
        println!("  FSCTL_GET_NTFS_VOLUME_DATA failed (os error {})", last_err());
        println!("\nNO-GO: not an NTFS volume, or the ioctl is unavailable.");
        return;
    }
    let bps = vd.BytesPerSector as usize;
    let bpc = vd.BytesPerCluster as i64;
    let rec_size = vd.BytesPerFileRecordSegment as usize;
    println!("  bytes/sector             : {bps}");
    println!("  bytes/cluster            : {bpc}");
    println!("  bytes/file record segment: {rec_size}");
    println!(
        "  clusters/record segment  : {} (0 is normal when a record is smaller than a cluster)",
        vd.ClustersPerFileRecordSegment
    );
    println!("  MftValidDataLength       : {} bytes", vd.MftValidDataLength);
    println!("  MftStartLcn              : {}", vd.MftStartLcn);
    println!(
        "  => up to {} MFT record slots",
        vd.MftValidDataLength / rec_size.max(1) as i64
    );
    if rec_size == 0 || !rec_size.is_power_of_two() {
        println!("\nNO-GO: implausible record size {rec_size}.");
        return;
    }

    // --- 2. the elevation gate --------------------------------------------
    println!("\n== 2. raw volume read access (the elevation gate) ==");
    let device = format!("\\\\.\\{letter}:");
    let vol_h = match open(
        &device,
        FILE_READ_DATA,
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN,
    ) {
        Ok(h) => {
            println!("  open {device} with FILE_READ_DATA -> OK");
            h
        }
        Err(5) => {
            println!("  open {device} with FILE_READ_DATA -> ACCESS DENIED (os error 5)");
            println!("\nNO-GO: run this from an elevated (Administrator) shell.");
            return;
        }
        Err(e) => {
            println!("  open {device} with FILE_READ_DATA -> failed (os error {e})");
            println!("\nNO-GO: unexpected failure opening the volume device.");
            return;
        }
    };

    // --- 3. extent map ----------------------------------------------------
    println!("\n== 3. $MFT extent map ==");
    let mft_path = format!("{letter}:\\$MFT");
    let extents = match open(&mft_path, 0, 0) {
        Ok(mh) => {
            let r = mft_extents(mh);
            // SAFETY: opened above and closed once.
            unsafe { CloseHandle(mh) };
            match r {
                Ok(e) => e,
                Err(err) => {
                    println!("  FSCTL_GET_RETRIEVAL_POINTERS failed (os error {err})");
                    println!("\nNO-GO: cannot map $MFT. A run-list decoder would be needed.");
                    // SAFETY: opened above and closed once.
                    unsafe { CloseHandle(vol_h) };
                    return;
                }
            }
        }
        Err(e) => {
            println!("  cannot open {mft_path} with access=0 (os error {e})");
            println!("\nNO-GO: cannot obtain a handle to $MFT.");
            // SAFETY: opened above and closed once.
            unsafe { CloseHandle(vol_h) };
            return;
        }
    };
    println!("  fragments: {}", extents.len());
    for (i, e) in extents.iter().take(8).enumerate() {
        println!(
            "    [{i}] lcn {:>12}  {:>10} clusters  ({:.1} MB)",
            e.lcn,
            e.clusters,
            (e.clusters * bpc) as f64 / 1e6
        );
    }
    if extents.len() > 8 {
        println!("    … and {} more", extents.len() - 8);
    }

    // --- 4. coverage check -------------------------------------------------
    let mapped: i64 = extents.iter().map(|e| e.clusters).sum::<i64>() * bpc;
    println!("\n== 4. coverage ==");
    println!("  mapped by extents  : {mapped} bytes");
    println!("  MftValidDataLength : {} bytes", vd.MftValidDataLength);
    if mapped < vd.MftValidDataLength {
        println!("  => SHORT by {} bytes", vd.MftValidDataLength - mapped);
        println!("\nNO-GO: the extent map does not cover the live MFT.");
        // SAFETY: opened above and closed once.
        unsafe { CloseHandle(vol_h) };
        return;
    }
    println!("  => covers the live MFT");

    // --- 5. read and validate record 0 ------------------------------------
    println!("\n== 5. read MFT record 0 from the raw device ==");
    let first = extents[0];
    let byte_off = first.lcn * bpc;
    // NO_BUFFERING needs an offset and length that are sector multiples.
    let read_len = rec_size.max(bps).next_multiple_of(bps);
    let mut buf = AlignedBuf::new(read_len, bps.max(4096));
    let mut pos = 0i64;
    // SAFETY: vol_h is a valid device handle; pos receives the new offset.
    let sought = unsafe { SetFilePointerEx(vol_h, byte_off, &mut pos, FILE_BEGIN) };
    if sought == 0 {
        println!("  seek to {byte_off} failed (os error {})", last_err());
        println!("\nNO-GO: cannot seek the volume device.");
        // SAFETY: opened above and closed once.
        unsafe { CloseHandle(vol_h) };
        return;
    }
    let mut got = 0u32;
    // SAFETY: buf is a live sector-aligned allocation of read_len bytes.
    let rok = unsafe {
        ReadFile(
            vol_h,
            buf.as_mut_slice().as_mut_ptr(),
            read_len as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    if rok == 0 {
        println!("  ReadFile failed (os error {})", last_err());
        println!("\nNO-GO: cannot read the volume device even with a handle.");
        // SAFETY: opened above and closed once.
        unsafe { CloseHandle(vol_h) };
        return;
    }
    println!("  read {got} bytes at offset {byte_off}");

    let mut rec = buf.as_slice()[..rec_size].to_vec();
    match apply_fixups(&mut rec, bps) {
        Ok(()) => println!("  update-sequence fixups applied cleanly"),
        Err(e) => {
            println!("  fixups FAILED: {e}");
            println!("\nNO-GO: record 0 did not validate; the read path is wrong.");
            // SAFETY: opened above and closed once.
            unsafe { CloseHandle(vol_h) };
            return;
        }
    }
    let self_num = u32::from_le_bytes(rec[0x2C..0x30].try_into().unwrap());
    let flags = u16::from_le_bytes(rec[0x16..0x18].try_into().unwrap());
    let name = first_file_name(&rec);
    println!("  self record number : {self_num} (expected 0)");
    println!(
        "  flags              : {flags:#06x} (bit0 in-use, bit1 directory)",
    );
    println!("  $FILE_NAME         : {name:?} (expected Some(\"$MFT\"))");
    let record_ok = self_num == 0 && flags & 1 == 1 && name.as_deref() == Some("$MFT");
    if !record_ok {
        println!("\nNO-GO: record 0 is not $MFT. Geometry or extent mapping is off.");
        // SAFETY: opened above and closed once.
        unsafe { CloseHandle(vol_h) };
        return;
    }

    // --- 6. sequential bandwidth ------------------------------------------
    println!("\n== 6. sequential read bandwidth (sizes the real scan) ==");
    let chunk = 16 * 1024 * 1024;
    let mut big = AlignedBuf::new(chunk, bps.max(4096));
    let mut total = 0u64;
    let start = Instant::now();
    let mut off = byte_off;
    let cap = (first.clusters * bpc).min(chunk as i64 * 8);
    while total < cap as u64 {
        let mut p = 0i64;
        // SAFETY: vol_h is valid; p receives the new offset.
        unsafe { SetFilePointerEx(vol_h, off, &mut p, FILE_BEGIN) };
        let mut n = 0u32;
        // SAFETY: big is a live sector-aligned allocation of `chunk` bytes.
        let r = unsafe {
            ReadFile(
                vol_h,
                big.as_mut_slice().as_mut_ptr(),
                chunk as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if r == 0 || n == 0 {
            break;
        }
        total += n as u64;
        off += n as i64;
    }
    let secs = start.elapsed().as_secs_f64();
    let mbps = total as f64 / 1e6 / secs.max(1e-9);
    println!("  read {:.1} MB in {secs:.3} s = {mbps:.0} MB/s", total as f64 / 1e6);
    println!(
        "  => projected full MFT read: {:.1} s for {:.2} GB",
        vd.MftValidDataLength as f64 / 1e6 / mbps.max(1.0),
        vd.MftValidDataLength as f64 / 1e9
    );

    // SAFETY: opened above and closed once.
    unsafe { CloseHandle(vol_h) };

    println!("\nGO: every step succeeded. The MFT fast path is buildable on this volume.");
}
