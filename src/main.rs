use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use disktree::app::App;
use disktree::index::{self, ScanProgress, Tree};
use disktree::ui;

#[derive(Parser, Debug)]
#[command(
    name = "disktree",
    version,
    about = "Disk usage TUI: scan, browse, and delete what's taking space"
)]
struct Args {
    /// Directory to scan. Defaults to the system root.
    #[arg(short, long)]
    path: Option<String>,

    /// Threads used to enumerate directories. Defaults to twice the core count.
    ///
    /// Only affects the directory walk, not the MFT fast scan.
    #[arg(short = 'j', long)]
    threads: Option<usize>,

    /// Scan the whole volume via the NTFS MFT even when a subdirectory was given.
    #[arg(long, conflicts_with = "no_mft")]
    mft: bool,

    /// Never use the MFT fast scan; always walk directories.
    #[arg(long)]
    no_mft: bool,

    /// Concurrent reads during an MFT scan. Defaults to 8.
    #[arg(long)]
    mft_readers: Option<usize>,

    /// Do not relaunch elevated, even when that would enable the fast scan.
    #[arg(long)]
    no_elevate: bool,

    /// Set on the relaunched elevated process so it never tries to elevate again.
    #[arg(long, hide = true)]
    elevated_child: bool,
}

/// Where to start when the user names no path.
fn default_root() -> &'static str {
    if cfg!(windows) { "C:\\" } else { "/" }
}

/// Which scanner to run, and what to tell the user about the choice.
struct Plan {
    /// `Some` when the MFT fast path is available and wanted. Shared rather than
    /// owned so a rescan can reuse it without re-probing the volume.
    #[cfg(windows)]
    volume: Option<std::sync::Arc<disktree::mft::Volume>>,
    /// Shown in the browser's status line once the scan finishes.
    notice: Option<String>,
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let path = PathBuf::from(
        args.path
            .clone()
            .unwrap_or_else(|| default_root().to_owned()),
    );
    if !path.is_dir() {
        eprintln!("'{}' is not a directory", path.display());
        std::process::exit(1);
    }

    // Decide, and possibly relaunch elevated, *before* taking over the terminal:
    // a UAC prompt or an early exit must not leave the console in raw mode.
    let plan = choose_scanner(&path, &args);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, path, &args, plan);
    ratatui::restore();
    result
}

#[cfg(not(windows))]
fn choose_scanner(_path: &Path, _args: &Args) -> Plan {
    Plan { notice: None }
}

#[cfg(windows)]
fn choose_scanner(path: &Path, args: &Args) -> Plan {
    use disktree::mft::{self, Capability};

    if args.no_mft {
        return Plan {
            volume: None,
            notice: None,
        };
    }

    match mft::probe(path) {
        Capability::Ready(vol) => {
            // The MFT describes an entire volume. Reading it to show one
            // subdirectory would be slower than walking that subdirectory, so the
            // fast path is for whole-volume scans unless explicitly asked for.
            let is_root = path == vol.mount();
            if is_root || args.mft {
                let secs_hint = vol.mft_bytes() as f64 / 800e6;
                Plan {
                    notice: Some(format!(
                        "MFT scan: {:.2} GB of $MFT across {} fragments, up to {} records (~{:.0}s)",
                        vol.mft_bytes() as f64 / 1e9,
                        vol.fragments(),
                        vol.record_slots(),
                        secs_hint,
                    )),
                    volume: Some(std::sync::Arc::from(vol)),
                }
            } else {
                Plan {
                    volume: None,
                    notice: Some(
                        "walked directories; pass --mft to read the whole volume instead"
                            .to_owned(),
                    ),
                }
            }
        }

        Capability::NeedsElevation { letter, mft_bytes } => {
            let gb = mft_bytes as f64 / 1e9;
            // Only relaunch when the fast path would actually be used, i.e. for a
            // whole-volume scan, and never from an already-elevated child.
            let wants_fast = args.mft || is_volume_root(path);
            if !args.no_elevate && !args.elevated_child && wants_fast {
                match relaunch_elevated() {
                    Ok(()) => {
                        println!(
                            "Relaunching as administrator to read {letter}:'s MFT ({gb:.2} GB) \
                             in a new window."
                        );
                        std::process::exit(0);
                    }
                    Err(why) => Plan {
                        volume: None,
                        notice: Some(format!(
                            "not elevated ({why}); walked directories instead. \
                             An MFT scan of {letter}: would read {gb:.2} GB"
                        )),
                    },
                }
            } else {
                Plan {
                    volume: None,
                    notice: Some(format!(
                        "not elevated; walked directories. Run as administrator for an MFT \
                         scan of {letter}: ({gb:.2} GB)"
                    )),
                }
            }
        }

        Capability::NotNtfs { filesystem } => Plan {
            volume: None,
            notice: Some(format!(
                "{filesystem} is not NTFS, so there is no MFT; walked directories"
            )),
        },

        Capability::Unsupported(why) => Plan {
            volume: None,
            notice: Some(format!("MFT scan unavailable ({why}); walked directories")),
        },
    }
}

/// True when `path` is the root of its own volume, e.g. `C:\`.
#[cfg(windows)]
fn is_volume_root(path: &Path) -> bool {
    path.parent().is_none()
}

/// Relaunch this process elevated, forwarding the original arguments.
///
/// A console process cannot gain privileges in place, so this starts a new
/// elevated instance with its own console window and the caller exits. The child
/// carries `--elevated-child` so a denied or failed probe can never produce an
/// elevation loop.
#[cfg(windows)]
fn relaunch_elevated() -> Result<(), String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut params: Vec<String> = std::env::args().skip(1).collect();
    params.push("--elevated-child".to_owned());
    let joined = params
        .iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    let wide = |s: OsString| -> Vec<u16> {
        s.encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let verb = wide(OsString::from("runas"));
    let file = wide(exe.into_os_string());
    let args_w = wide(OsString::from(joined));

    // SAFETY: every pointer is a NUL-terminated UTF-16 buffer that outlives the
    // call; the remaining arguments are scalars.
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            args_w.as_ptr(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    // ShellExecuteW returns a pseudo-HINSTANCE; anything above 32 means success.
    if rc as isize > 32 {
        Ok(())
    } else if rc as isize == 5 {
        Err("elevation declined".to_owned())
    } else {
        Err(format!("ShellExecuteW returned {}", rc as isize))
    }
}

fn run(terminal: &mut DefaultTerminal, path: PathBuf, args: &Args, plan: Plan) -> io::Result<()> {
    loop {
        let Some(tree) = run_scan(terminal, &path, args, &plan)? else {
            return Ok(());
        };
        let mut app = App::new(tree);
        if let Some(n) = plan.notice.clone() {
            app.status = Some(n);
        }
        run_browse(terminal, &mut app)?;
        if !app.rescan {
            return Ok(());
        }
    }
}

/// Scan on a background thread while showing live progress.
/// Returns None if the user cancelled.
fn run_scan(
    terminal: &mut DefaultTerminal,
    path: &Path,
    args: &Args,
    plan: &Plan,
) -> io::Result<Option<Tree>> {
    let progress = Arc::new(ScanProgress::default());
    let (tx, rx) = mpsc::channel();

    #[cfg(windows)]
    let mode = if plan.volume.is_some() {
        ui::ScanMode::Mft
    } else {
        ui::ScanMode::Walk
    };
    #[cfg(not(windows))]
    let mode = {
        // Only the Windows build consults the plan; off Windows there is no MFT.
        let _ = plan;
        ui::ScanMode::Walk
    };

    {
        let progress = Arc::clone(&progress);
        let root = path.to_path_buf();
        let threads = args.threads;

        #[cfg(windows)]
        let readers = args.mft_readers;
        #[cfg(windows)]
        let volume = plan.volume.clone();

        thread::spawn(move || {
            #[cfg(windows)]
            let tree = match volume {
                Some(vol) => disktree::mft::scan(&vol, &progress, readers)
                    .unwrap_or_else(|_| index::scan_with_threads(root, &progress, threads)),
                None => index::scan_with_threads(root, &progress, threads),
            };
            #[cfg(not(windows))]
            let tree = index::scan_with_threads(root, &progress, threads);
            let _ = tx.send(tree);
        });
    }

    let start = Instant::now();
    loop {
        if let Ok(tree) = rx.try_recv() {
            return Ok(Some(tree));
        }
        terminal.draw(|f| ui::draw_scanning(f, path, &progress, start.elapsed(), mode))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            if key.kind == KeyEventKind::Press
                && (key.code == KeyCode::Char('q') || key.code == KeyCode::Esc || ctrl_c)
            {
                return Ok(None);
            }
        }
    }
}

fn run_browse(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Confirmed deletes run here so the "Deleting…" frame above is visible
        // while the (possibly slow) filesystem operations happen.
        if let Some((targets, permanent)) = app.pending_delete.take() {
            app.perform_delete(&targets, permanent);
            continue;
        }

        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key);
        }
        if app.quit || app.rescan {
            return Ok(());
        }
    }
}
