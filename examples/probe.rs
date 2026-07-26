//! Privilege/capability probe for fast-scan strategies.
//! `cargo run --release --example probe -- C`
//!
//! Answers, on the current machine and at the current privilege level:
//!   1. Which access masks can open a raw volume handle (\\.\X:)?
//!   2. Does FSCTL_QUERY_USN_JOURNAL work, and at what access level?
//!   3. Does FSCTL_ENUM_USN_DATA work, and do its records carry a file SIZE?
//!   4. Can we read the NTFS boot sector (needed to locate the MFT)?

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, ReadFile,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00B3;
const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
const GENERIC_READ: u32 = 0x8000_0000;

fn open_volume(letter: char, access: u32) -> Result<HANDLE, u32> {
    let path: Vec<u16> = std::ffi::OsString::from(format!("\\\\.\\{letter}:"))
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: path is a NUL-terminated UTF-16 string; all other args are plain scalars.
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        Err(io::Error::last_os_error().raw_os_error().unwrap_or(-1) as u32)
    } else {
        Ok(h)
    }
}

fn main() {
    let letter = std::env::args()
        .nth(1)
        .and_then(|s| s.chars().next())
        .unwrap_or('C')
        .to_ascii_uppercase();

    println!("probing volume {letter}:\n");

    let masks: [(&str, u32); 4] = [
        ("access=0 (query only)", 0),
        ("FILE_READ_ATTRIBUTES", FILE_READ_ATTRIBUTES),
        ("FILE_READ_DATA", FILE_READ_DATA),
        ("GENERIC_READ", GENERIC_READ),
    ];

    let mut handles: Vec<(&str, HANDLE)> = Vec::new();
    println!("== 1. raw volume handle ==");
    for (label, mask) in masks {
        match open_volume(letter, mask) {
            Ok(h) => {
                println!("  {label:<24} -> OK");
                handles.push((label, h));
            }
            Err(e) => println!("  {label:<24} -> FAILED (os error {e})"),
        }
    }

    for &(label, h) in &handles {
        println!("\n== 2/3. FSCTLs on handle ({label}) ==");

        // USN_JOURNAL_DATA_V2 is 80 bytes.
        let mut out = [0u8; 80];
        let mut returned = 0u32;
        // SAFETY: h is a valid volume handle; out is a 80-byte buffer matching the
        // documented USN_JOURNAL_DATA_V2 size; returned receives the byte count.
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                out.as_mut_ptr() as *mut c_void,
                out.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            let jid = u64::from_le_bytes(out[0..8].try_into().unwrap());
            println!("  FSCTL_QUERY_USN_JOURNAL -> OK ({returned} bytes) journal id {jid:#x}");
        } else {
            println!(
                "  FSCTL_QUERY_USN_JOURNAL -> FAILED (os error {})",
                io::Error::last_os_error().raw_os_error().unwrap_or(-1)
            );
        }

        // MFT_ENUM_DATA_V0 { StartFileReferenceNumber: u64, LowUsn: i64, HighUsn: i64 }
        let mut input = [0u8; 24];
        input[0..8].copy_from_slice(&0u64.to_le_bytes());
        input[8..16].copy_from_slice(&0i64.to_le_bytes());
        input[16..24].copy_from_slice(&i64::MAX.to_le_bytes());
        let mut buf = vec![0u8; 64 * 1024];
        let mut returned = 0u32;
        // SAFETY: input is a correctly sized MFT_ENUM_DATA_V0; buf is a 64 KiB
        // output buffer; both pointers outlive the call.
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_ENUM_USN_DATA,
                input.as_ptr() as *const c_void,
                input.len() as u32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            println!("  FSCTL_ENUM_USN_DATA    -> OK, {returned} bytes");
            let mut off = 8usize; // first 8 bytes = next start file reference number
            let mut shown = 0;
            while off + 60 <= returned as usize && shown < 5 {
                let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                if len == 0 || off + len > returned as usize {
                    break;
                }
                let major = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap());
                let frn = u64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap());
                let parent = u64::from_le_bytes(buf[off + 16..off + 24].try_into().unwrap());
                let attrs = u32::from_le_bytes(buf[off + 52..off + 56].try_into().unwrap());
                let name_len =
                    u16::from_le_bytes(buf[off + 56..off + 58].try_into().unwrap()) as usize;
                let name_off =
                    u16::from_le_bytes(buf[off + 58..off + 60].try_into().unwrap()) as usize;
                let name = String::from_utf16_lossy(
                    &buf[off + name_off..off + name_off + name_len]
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect::<Vec<u16>>(),
                );
                println!(
                    "    v{major} len={len:<4} frn={:<10} parent={:<10} attrs={attrs:#010x} name={name:?}",
                    frn & 0x0000_FFFF_FFFF_FFFF,
                    parent & 0x0000_FFFF_FFFF_FFFF,
                );
                off += len;
                shown += 1;
            }
            println!("    NOTE: USN_RECORD_V2 is 60 bytes of header + name. Fields are");
            println!("    RecordLength/Version/FRN/ParentFRN/Usn/TimeStamp/Reason/SourceInfo/");
            println!("    SecurityId/FileAttributes/FileNameLength/FileNameOffset.");
            println!("    There is NO file-size field -> USN enum alone cannot size files.");
        } else {
            println!(
                "  FSCTL_ENUM_USN_DATA    -> FAILED (os error {})",
                io::Error::last_os_error().raw_os_error().unwrap_or(-1)
            );
        }
    }

    // Boot sector read requires FILE_READ_DATA on the volume.
    println!("\n== 4. NTFS boot sector (locates the MFT) ==");
    match handles
        .iter()
        .find(|(l, _)| *l == "FILE_READ_DATA" || *l == "GENERIC_READ")
    {
        Some(&(label, h)) => {
            let mut sector = [0u8; 512];
            let mut read = 0u32;
            // SAFETY: h has FILE_READ_DATA; sector is 512 bytes, matching the
            // volume's sector size for the initial boot-sector read.
            let ok = unsafe {
                ReadFile(
                    h,
                    sector.as_mut_ptr(),
                    sector.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 {
                let oem = String::from_utf8_lossy(&sector[3..11]).to_string();
                let bytes_per_sector = u16::from_le_bytes(sector[11..13].try_into().unwrap());
                let sectors_per_cluster = sector[13];
                let mft_lcn = u64::from_le_bytes(sector[48..56].try_into().unwrap());
                let clusters_per_record = sector[64] as i8;
                println!("  read {read} bytes via {label}");
                println!("  OEM id             : {oem:?}");
                println!("  bytes/sector       : {bytes_per_sector}");
                println!("  sectors/cluster    : {sectors_per_cluster}");
                println!("  $MFT start LCN     : {mft_lcn}");
                println!("  clusters/MFT record: {clusters_per_record} (negative => 2^-n bytes)");
            } else {
                println!(
                    "  ReadFile FAILED (os error {})",
                    io::Error::last_os_error().raw_os_error().unwrap_or(-1)
                );
            }
        }
        None => println!("  skipped: no handle with FILE_READ_DATA could be opened"),
    }

    for (_, h) in handles {
        // SAFETY: each handle came from a successful CreateFileW and is closed once.
        unsafe { CloseHandle(h) };
    }
}
