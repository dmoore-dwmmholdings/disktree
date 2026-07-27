//! Volume access for the MFT scan: capability probe, extent map, and the
//! concurrent aligned reader.
//!
//! Everything here needs a real volume, so it is deliberately thin — the logic
//! that decides whether results are *correct* lives in the parent module as pure
//! functions with unit tests.

use std::ffi::{OsString, c_void};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_BEGIN, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING,
    FILE_FLAG_SEQUENTIAL_SCAN, FILE_READ_DATA, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetVolumeInformationW, GetVolumePathNameW, OPEN_EXISTING, ReadFile, SetFilePointerEx,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_GET_RETRIEVAL_POINTERS, NTFS_VOLUME_DATA_BUFFER,
};

use crate::index::{Node, ScanProgress, Tree};

use super::{Record, RecordError, apply_fixups, build_tree, merge_extension_records, parse_record};

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_MORE_DATA: i32 = 234;

/// Bytes per read request. 16 MiB was the best measured size-per-pinned-memory
/// tradeoff; larger requests gained a few percent for four times the memory.
const CHUNK: usize = 16 * 1024 * 1024;

/// Concurrent readers when the caller does not choose. See [`calibrate_readers`]
/// for how this was picked; a synchronous read loop keeps only one request
/// outstanding, which leaves an NVMe device well short of its sequential peak.
pub const DEFAULT_READERS: usize = 8;

/// What the MFT fast path can do for a given path, right now.
pub enum Capability {
    /// Everything needed is available.
    Ready(Box<Volume>),
    /// NTFS, but reading the raw volume was denied. Carries the volume letter and
    /// how many bytes an MFT scan would read, so the caller can say what
    /// elevating would buy.
    NeedsElevation {
        /// Volume letter, for the message telling the user what elevating buys.
        letter: char,
        /// Bytes an MFT scan would read, i.e. the size of the live `$MFT`.
        mft_bytes: u64,
    },
    /// Not an NTFS volume, so there is no MFT to read.
    NotNtfs {
        /// Filesystem name as reported by Windows, e.g. `ReFS` or `exFAT`.
        filesystem: String,
    },
    /// Something else went wrong; the caller should fall back quietly.
    Unsupported(String),
}

/// One `$MFT` fragment, in clusters.
#[derive(Debug, Clone, Copy)]
struct Extent {
    lcn: i64,
    clusters: i64,
}

/// A volume ready to be scanned via its MFT.
pub struct Volume {
    /// `\\.\C:` — the raw device.
    device: String,
    /// `C:\` — the mount point, used as the tree's root path.
    mount: PathBuf,
    bytes_per_sector: usize,
    bytes_per_cluster: u64,
    record_size: usize,
    /// Live length of `$MFT`. Slots past this are stale or zero.
    mft_valid_len: u64,
    extents: Vec<Extent>,
}

impl Volume {
    /// Mount point of this volume, e.g. `C:\`.
    pub fn mount(&self) -> &Path {
        &self.mount
    }

    /// Total record slots the scan will examine. An upper bound on file count,
    /// since it includes free, deleted, reserved and extension records.
    pub fn record_slots(&self) -> u64 {
        self.mft_valid_len / self.record_size as u64
    }

    /// Bytes the scan will read.
    pub fn mft_bytes(&self) -> u64 {
        self.mft_valid_len
    }

    /// How many fragments `$MFT` is in.
    pub fn fragments(&self) -> usize {
        self.extents.len()
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

fn open(path: &str, access: u32, flags: u32) -> Result<Handle, i32> {
    let w = wide(path);
    // SAFETY: w is a NUL-terminated UTF-16 string; the rest are scalars.
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
        Ok(Handle(h))
    }
}

/// Owns a Win32 handle so every early return still closes it.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful CreateFileW and Drop runs once.
        unsafe { CloseHandle(self.0) };
    }
}

/// A heap buffer whose address is sector-aligned, which
/// `FILE_FLAG_NO_BUFFERING` requires along with aligned offsets and lengths.
struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
    layout: std::alloc::Layout,
}

impl AlignedBuf {
    fn new(len: usize, align: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, align.max(8)).expect("valid layout");
        // SAFETY: len is nonzero and align is a power of two from volume geometry.
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null(), "failed to allocate {len} bytes");
        AlignedBuf { ptr, len, layout }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr is a live allocation of exactly len bytes, uniquely borrowed.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated with this exact layout and freed once.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

/// Resolve the mount point that contains `path`, e.g. `C:\`.
fn mount_point(path: &Path) -> Result<PathBuf, i32> {
    let w: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut out = vec![0u16; 260];
    // SAFETY: w is NUL-terminated; out has room for the documented maximum.
    let ok = unsafe { GetVolumePathNameW(w.as_ptr(), out.as_mut_ptr(), out.len() as u32) };
    if ok == 0 {
        return Err(last_err());
    }
    let n = out.iter().position(|&c| c == 0).unwrap_or(out.len());
    Ok(PathBuf::from(OsString::from_wide(&out[..n])))
}

/// Filesystem name for a mount point, e.g. `NTFS`.
fn filesystem_name(mount: &Path) -> Result<String, i32> {
    let w: Vec<u16> = mount
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut fs = vec![0u16; 32];
    // SAFETY: w is NUL-terminated; every out-pointer we pass is either null or
    // sized by the length argument beside it.
    let ok = unsafe {
        GetVolumeInformationW(
            w.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            fs.len() as u32,
        )
    };
    if ok == 0 {
        return Err(last_err());
    }
    let n = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
    Ok(String::from_utf16_lossy(&fs[..n]))
}

/// Ask NTFS where `$MFT` lives. Requires no access rights on the handle, which
/// is why `$MFT` is opened with `access = 0`.
fn mft_extents(h: &Handle) -> Result<Vec<Extent>, i32> {
    let mut out = Vec::new();
    let mut start_vcn: i64 = 0;
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let input = start_vcn;
        let mut returned = 0u32;
        // SAFETY: input is a STARTING_VCN_INPUT_BUFFER (a single i64); buf
        // receives RETRIEVAL_POINTERS_BUFFER and is read via checked offsets.
        let ok = unsafe {
            DeviceIoControl(
                h.0,
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

        let count = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        let mut vcn = i64::from_le_bytes(buf[8..16].try_into().unwrap());
        let before = vcn;
        for i in 0..count {
            let base = 16 + i * 16;
            if base + 16 > buf.len() {
                break;
            }
            let next_vcn = i64::from_le_bytes(buf[base..base + 8].try_into().unwrap());
            let lcn = i64::from_le_bytes(buf[base + 8..base + 16].try_into().unwrap());
            let clusters = next_vcn - vcn;
            // LCN < 0 marks a sparse run, which $MFT never has.
            if lcn >= 0 && clusters > 0 {
                out.push(Extent { lcn, clusters });
            }
            vcn = next_vcn;
        }
        if ok != 0 || vcn == before {
            return Ok(out);
        }
        start_vcn = vcn;
    }
}

/// Decide what the MFT fast path can do for `path`.
///
/// Fails closed: anything unexpected yields a variant that tells the caller to
/// use the directory walk instead. Never panics and never prompts.
pub fn probe(path: &Path) -> Capability {
    let mount = match mount_point(path) {
        Ok(m) => m,
        Err(e) => return Capability::Unsupported(format!("cannot resolve volume (os error {e})")),
    };

    // Require NTFS by name. `ERROR_INVALID_FUNCTION` from an ioctl cannot
    // distinguish "not NTFS" from "wrong kind of handle", so it must not be used
    // as the test.
    match filesystem_name(&mount) {
        Ok(fs) if fs == "NTFS" => {}
        Ok(fs) => return Capability::NotNtfs { filesystem: fs },
        Err(e) => {
            return Capability::Unsupported(format!("cannot identify filesystem (os error {e})"));
        }
    }

    let letter = mount
        .to_string_lossy()
        .chars()
        .next()
        .unwrap_or('C')
        .to_ascii_uppercase();

    // Geometry, via the mount-point root. Works without elevation.
    let root_h = match open(&mount.to_string_lossy(), 0, FILE_FLAG_BACKUP_SEMANTICS) {
        Ok(h) => h,
        Err(e) => {
            return Capability::Unsupported(format!("cannot open volume root (os error {e})"));
        }
    };
    let mut vd = NTFS_VOLUME_DATA_BUFFER::default();
    let mut returned = 0u32;
    // SAFETY: root_h is a directory handle; vd is exactly the struct this ioctl
    // fills and its size is passed from size_of.
    let ok = unsafe {
        DeviceIoControl(
            root_h.0,
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            &mut vd as *mut _ as *mut c_void,
            size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    drop(root_h);
    if ok == 0 {
        return Capability::Unsupported(format!(
            "volume data query failed (os error {})",
            last_err()
        ));
    }

    // Take the stride from BytesPerFileRecordSegment. ClustersPerFileRecordSegment
    // is 0 whenever a record is smaller than a cluster, which is the normal case,
    // so anything derived from it divides by zero.
    let record_size = vd.BytesPerFileRecordSegment as usize;
    let bytes_per_sector = vd.BytesPerSector as usize;
    let bytes_per_cluster = vd.BytesPerCluster as u64;
    let mft_valid_len = vd.MftValidDataLength.max(0) as u64;
    if record_size == 0 || !record_size.is_power_of_two() || bytes_per_sector == 0 {
        return Capability::Unsupported(format!("implausible geometry: record {record_size}"));
    }
    // A u32 record index must be able to address every slot.
    if mft_valid_len / record_size as u64 > u32::MAX as u64 {
        return Capability::Unsupported("MFT has more slots than a u32 can index".to_owned());
    }

    // The elevation gate. Only ERROR_ACCESS_DENIED means "needs elevation".
    let device = format!("\\\\.\\{letter}:");
    let vol_h = match open(
        &device,
        FILE_READ_DATA,
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN,
    ) {
        Ok(h) => h,
        Err(ERROR_ACCESS_DENIED) => {
            return Capability::NeedsElevation {
                letter,
                mft_bytes: mft_valid_len,
            };
        }
        Err(e) => return Capability::Unsupported(format!("cannot open {device} (os error {e})")),
    };
    drop(vol_h);

    let mft_path = format!("{letter}:\\$MFT");
    let extents = match open(&mft_path, 0, 0) {
        Ok(h) => match mft_extents(&h) {
            Ok(e) if !e.is_empty() => e,
            Ok(_) => return Capability::Unsupported("$MFT reported no extents".to_owned()),
            Err(e) => {
                return Capability::Unsupported(format!("cannot map $MFT (os error {e})"));
            }
        },
        Err(e) => return Capability::Unsupported(format!("cannot open $MFT (os error {e})")),
    };

    let mapped: u64 = extents.iter().map(|e| e.clusters as u64).sum::<u64>() * bytes_per_cluster;
    if mapped < mft_valid_len {
        return Capability::Unsupported(format!(
            "$MFT extents cover {mapped} of {mft_valid_len} bytes"
        ));
    }

    Capability::Ready(Box::new(Volume {
        device,
        mount,
        bytes_per_sector,
        bytes_per_cluster,
        record_size,
        mft_valid_len,
        extents,
    }))
}

/// One contiguous read: where it lands on the device, and which MFT record it starts at.
#[derive(Clone, Copy)]
struct Job {
    device_offset: i64,
    len: usize,
    first_record: u32,
}

/// Split the MFT into device-contiguous, record-aligned read jobs.
///
/// Cluster size is always a multiple of record size, so extents hold whole
/// records and a chunk boundary inside an extent never splits one.
fn plan_jobs(vol: &Volume) -> Vec<Job> {
    let mut jobs = Vec::new();
    let mut mft_off: u64 = 0;
    for e in &vol.extents {
        if mft_off >= vol.mft_valid_len {
            break;
        }
        let extent_bytes = e.clusters as u64 * vol.bytes_per_cluster;
        let usable = extent_bytes.min(vol.mft_valid_len - mft_off);
        let mut done: u64 = 0;
        while done < usable {
            let len = CHUNK.min((usable - done) as usize);
            jobs.push(Job {
                device_offset: (e.lcn as u64 * vol.bytes_per_cluster + done) as i64,
                len,
                first_record: ((mft_off + done) / vol.record_size as u64) as u32,
            });
            done += len as u64;
        }
        mft_off += extent_bytes;
    }
    jobs
}

/// Read and parse one job's worth of records.
fn run_job(
    h: &Handle,
    buf: &mut AlignedBuf,
    job: Job,
    vol: &Volume,
    out: &mut Vec<(u32, Record)>,
    torn: &mut u64,
) -> Result<(), i32> {
    let mut pos = 0i64;
    // SAFETY: h is this thread's own device handle; pos receives the new offset.
    if unsafe { SetFilePointerEx(h.0, job.device_offset, &mut pos, FILE_BEGIN) } == 0 {
        return Err(last_err());
    }
    // NO_BUFFERING requires a sector-multiple length; job lengths are cluster
    // multiples, and clusters are sector multiples.
    let want = job.len;
    let mut filled = 0usize;
    while filled < want {
        let mut got = 0u32;
        // SAFETY: buf is a live sector-aligned allocation of at least `want`
        // bytes, uniquely borrowed here.
        let ok = unsafe {
            ReadFile(
                h.0,
                buf.as_mut_slice()[filled..].as_mut_ptr(),
                (want - filled) as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_err());
        }
        if got == 0 {
            break;
        }
        filled += got as usize;
    }

    let rec_size = vol.record_size;
    let data = &mut buf.as_mut_slice()[..filled];
    for (i, rec) in data.chunks_exact_mut(rec_size).enumerate() {
        let number = job.first_record + i as u32;
        match apply_fixups(rec, vol.bytes_per_sector) {
            Ok(()) => {}
            Err(RecordError::TornRead) => {
                *torn += 1;
                continue;
            }
            Err(_) => continue,
        }
        if let Ok(r) = parse_record(rec, number) {
            out.push((number, r));
        }
    }
    Ok(())
}

/// Scan a volume through its MFT and return the same [`Tree`] shape the
/// directory walk produces.
pub fn scan(vol: &Volume, progress: &ScanProgress, readers: Option<usize>) -> io::Result<Tree> {
    let start = Instant::now();
    let jobs = plan_jobs(vol);
    let n_readers = readers
        .unwrap_or(DEFAULT_READERS)
        .clamp(1, 64)
        .min(jobs.len().max(1));
    let slots = (vol.mft_valid_len / vol.record_size as u64) as usize + 1;

    if let Ok(mut cur) = progress.current.try_lock() {
        *cur = format!("reading $MFT ({} fragments)", vol.extents.len());
    }

    let next = AtomicUsize::new(0);
    let torn_total = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);

    // Each reader owns its handle and buffer: a shared file pointer would
    // serialise the very seeks we are overlapping.
    let collected: Vec<Vec<(u32, Record)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..n_readers)
            .map(|_| {
                s.spawn(|| {
                    let mut mine: Vec<(u32, Record)> = Vec::new();
                    let Ok(h) = open(
                        &vol.device,
                        FILE_READ_DATA,
                        FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN,
                    ) else {
                        failed.fetch_add(1, Ordering::Relaxed);
                        return mine;
                    };
                    let mut buf = AlignedBuf::new(CHUNK, vol.bytes_per_sector.max(4096));
                    let mut torn = 0u64;
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&job) = jobs.get(i) else { break };
                        if run_job(&h, &mut buf, job, vol, &mut mine, &mut torn).is_err() {
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                        progress.bytes.fetch_add(job.len as u64, Ordering::Relaxed);
                        progress
                            .dirs
                            .fetch_add((job.len / vol.record_size) as u64, Ordering::Relaxed);
                    }
                    torn_total.fetch_add(torn as usize, Ordering::Relaxed);
                    mine
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    });

    // Scatter into a slot array. Sequence numbers come along so build_tree can
    // reject a parent reference whose slot has since been recycled.
    let mut records: Vec<Option<Record>> = vec![None; slots];
    let mut sequences: Vec<u16> = vec![0; slots];
    for batch in collected {
        for (n, r) in batch {
            if let Some(slot) = records.get_mut(n as usize) {
                sequences[n as usize] = r.parent_seq;
                *slot = Some(r);
            }
        }
    }

    let merged = merge_extension_records(&mut records);
    let nodes = build_tree(&records, &sequences);
    drop(records);

    let n_files: u64 = nodes.iter().filter(|n| !n.is_dir).count() as u64;
    let tree = to_tree(
        nodes,
        vol.mount.clone(),
        torn_total.load(Ordering::Relaxed) as u64 + failed.load(Ordering::Relaxed) as u64,
        start.elapsed().as_secs_f64(),
    );

    progress.files.store(n_files, Ordering::Relaxed);
    if let Ok(mut cur) = progress.current.try_lock() {
        *cur = format!("merged {merged} extension records");
    }
    Ok(tree)
}

/// Convert breadth-first MFT nodes into an aggregated [`Tree`].
///
/// Breadth-first order guarantees a parent precedes its children, so subtree
/// sizes and counts roll up in a single reverse pass.
fn to_tree(nodes: Vec<super::TreeNode>, mount: PathBuf, errors: u64, secs: f64) -> Tree {
    let mut out: Vec<Node> = Vec::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        out.push(Node {
            // The root carries a display string rather than a path component.
            name: if i == 0 {
                OsString::from(mount.display().to_string())
            } else {
                n.name.clone()
            },
            is_dir: n.is_dir,
            size: n.size,
            n_files: u64::from(!n.is_dir),
            parent: if i == 0 { None } else { Some(n.parent) },
            children: Vec::new(),
            deleted: false,
        });
    }
    for i in (1..out.len()).rev() {
        let p = nodes[i].parent;
        let (size, files) = (out[i].size, out[i].n_files);
        out[p].size += size;
        out[p].n_files += files;
        out[p].children.push(i);
    }
    // Reverse pass pushed children in descending order; the browser sorts anyway,
    // but ascending keeps the arena's own order predictable for tests.
    for n in out.iter_mut() {
        n.children.reverse();
    }
    Tree {
        nodes: out,
        root: 0,
        root_path: mount,
        errors,
        scan_secs: secs,
    }
}

/// Measure read bandwidth against concurrency on a real file.
///
/// Used to choose [`DEFAULT_READERS`] without needing elevation: reads bypass
/// the cache via `FILE_FLAG_NO_BUFFERING`, so this exercises the device the same
/// way the MFT scan does. Returns `(readers, bytes_per_second)` pairs.
pub fn calibrate_readers(file: &Path, levels: &[usize]) -> io::Result<Vec<(usize, f64)>> {
    let path = file.to_string_lossy().into_owned();
    let total = std::fs::metadata(file)?.len() as usize;
    let usable = total.min(512 * 1024 * 1024) / CHUNK * CHUNK;
    if usable < CHUNK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("need at least {CHUNK} bytes to calibrate, got {total}"),
        ));
    }
    let mut results = Vec::new();
    for &readers in levels {
        let chunks = usable / CHUNK;
        if chunks < readers {
            continue;
        }
        let per = chunks / readers;
        let next = &AtomicUsize::new(0);
        let start = Instant::now();
        let read: usize = std::thread::scope(|s| {
            let hs: Vec<_> = (0..readers)
                .map(|_| {
                    let path = path.clone();
                    s.spawn(move || {
                        let Ok(h) = open(
                            &path,
                            FILE_READ_DATA,
                            FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN,
                        ) else {
                            return 0usize;
                        };
                        let mut buf = AlignedBuf::new(CHUNK, 4096);
                        let mut got_total = 0usize;
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= per * readers {
                                break;
                            }
                            let mut pos = 0i64;
                            // SAFETY: h is this thread's handle.
                            unsafe {
                                SetFilePointerEx(h.0, (i * CHUNK) as i64, &mut pos, FILE_BEGIN)
                            };
                            let mut got = 0u32;
                            // SAFETY: buf is a live aligned CHUNK-sized allocation.
                            let ok = unsafe {
                                ReadFile(
                                    h.0,
                                    buf.as_mut_slice().as_mut_ptr(),
                                    CHUNK as u32,
                                    &mut got,
                                    std::ptr::null_mut(),
                                )
                            };
                            if ok == 0 || got == 0 {
                                break;
                            }
                            got_total += got as usize;
                        }
                        got_total
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap_or(0)).sum()
        });
        let secs = start.elapsed().as_secs_f64().max(1e-9);
        results.push((readers, read as f64 / secs));
    }
    Ok(results)
}
