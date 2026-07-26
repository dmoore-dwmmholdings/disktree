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
    /// Raise it if scanning a cold, deep tree on a fast NVMe device, where
    /// workers spend their time blocked on the device rather than on the CPU.
    #[arg(short = 'j', long)]
    threads: Option<usize>,
}

/// Where to start when the user names no path.
fn default_root() -> &'static str {
    if cfg!(windows) { "C:\\" } else { "/" }
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let path = PathBuf::from(args.path.unwrap_or_else(|| default_root().to_owned()));
    if !path.is_dir() {
        eprintln!("'{}' is not a directory", path.display());
        std::process::exit(1);
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, path, args.threads);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal, path: PathBuf, threads: Option<usize>) -> io::Result<()> {
    loop {
        let Some(tree) = run_scan(terminal, &path, threads)? else {
            return Ok(());
        };
        let mut app = App::new(tree);
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
    threads: Option<usize>,
) -> io::Result<Option<Tree>> {
    let progress = Arc::new(ScanProgress::default());
    let (tx, rx) = mpsc::channel();
    {
        let progress = Arc::clone(&progress);
        let root = path.to_path_buf();
        thread::spawn(move || {
            let tree = index::scan_with_threads(root, &progress, threads);
            let _ = tx.send(tree);
        });
    }

    let start = Instant::now();
    loop {
        if let Ok(tree) = rx.try_recv() {
            return Ok(Some(tree));
        }
        terminal.draw(|f| ui::draw_scanning(f, path, &progress, start.elapsed()))?;
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
