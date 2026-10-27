use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

use crate::index::Tree;

pub enum Mode {
    Browse,
    Confirm {
        permanent: bool,
        targets: Vec<usize>,
    },
    ConfirmQuit,
}

/// A confirmed delete running on its own thread, so browsing continues.
pub struct DeleteJob {
    pub targets: Vec<usize>,
    pub permanent: bool,
    /// Scanned size of all targets: the denominator of the progress bar.
    pub total_bytes: u64,
    /// Bytes deleted so far, written by the worker.
    pub done_bytes: Arc<AtomicU64>,
    /// Path of the largest target, for the progress line.
    pub label: String,
    handle: JoinHandle<Vec<Outcome>>,
}

impl DeleteJob {
    pub fn fraction(&self) -> f64 {
        let done = self.done_bytes.load(Ordering::Relaxed);
        (done as f64 / self.total_bytes.max(1) as f64).min(1.0)
    }
}

enum Outcome {
    Deleted,
    Vanished,
    Failed(String),
}

pub struct App {
    pub tree: Tree,
    /// Node index of the directory currently being viewed.
    pub cwd: usize,
    /// Children of `cwd`, sorted by size descending.
    pub rows: Vec<usize>,
    pub selected: usize,
    /// Index of the first row drawn. Only the visible slice is ever built into
    /// widgets, so render cost is independent of how many children `cwd` has.
    pub offset: usize,
    pub marked: HashSet<usize>,
    pub mode: Mode,
    pub status: Option<String>,
    pub table_state: TableState,
    /// Deletes in flight, oldest first.
    pub jobs: Vec<DeleteJob>,
    pub quit: bool,
    pub rescan: bool,
}

impl App {
    pub fn new(tree: Tree) -> Self {
        let status = Some(format!(
            "Scanned {} files ({}) in {:.1} s",
            format_count(tree.nodes[tree.root].n_files),
            format_size(tree.nodes[tree.root].size),
            tree.scan_secs,
        ));
        let mut app = App {
            cwd: tree.root,
            tree,
            rows: Vec::new(),
            selected: 0,
            offset: 0,
            marked: HashSet::new(),
            mode: Mode::Browse,
            status,
            table_state: TableState::default(),
            jobs: Vec::new(),
            quit: false,
            rescan: false,
        };
        app.refresh_rows();
        app
    }

    fn refresh_rows(&mut self) {
        self.rows = self.tree.nodes[self.cwd].children.clone();
        self.rows.sort_unstable_by(|&a, &b| {
            let (na, nb) = (&self.tree.nodes[a], &self.tree.nodes[b]);
            nb.size.cmp(&na.size).then_with(|| na.name.cmp(&nb.name))
        });
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    /// Clamp `offset` so the selection is on screen, and return the range of
    /// rows to draw.
    ///
    /// Called every frame rather than only on key events, because a terminal
    /// resize changes `height` without any key being pressed.
    pub fn visible_range(&mut self, height: usize) -> std::ops::Range<usize> {
        let len = self.rows.len();
        if len == 0 || height == 0 {
            self.offset = 0;
            return 0..0;
        }
        if self.selected >= len {
            self.selected = len - 1;
        }
        // Scroll just far enough to bring the selection back into view.
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height {
            self.offset = self.selected - height + 1;
        }
        // Never leave blank rows below a full list.
        self.offset = self.offset.min(len.saturating_sub(height));
        self.offset..(self.offset + height).min(len)
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Browse => self.on_key_browse(key),
            Mode::Confirm { .. } => self.on_key_confirm(key),
            Mode::ConfirmQuit => match key.code {
                KeyCode::Esc => self.quit = true,
                KeyCode::Backspace => self.mode = Mode::Browse,
                _ => {}
            },
        }
    }

    fn on_key_browse(&mut self, key: KeyEvent) {
        self.status = None;
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true;
            }
            // Quitting kills the delete threads, so ask first while any run.
            KeyCode::Char('q') if !self.jobs.is_empty() => self.mode = Mode::ConfirmQuit,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => self.mode = Mode::ConfirmQuit,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-20),
            KeyCode::PageDown => self.move_selection(20),
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selected = self.rows.len().saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.enter_dir(),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.go_up(),
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('c') => {
                self.marked.clear();
                self.status = Some("Cleared all marks".to_owned());
            }
            KeyCode::Char('d') => self.request_delete(false),
            KeyCode::Char('D') => self.request_delete(true),
            // A rescan replaces the tree the running jobs report into.
            KeyCode::Char('r') if !self.jobs.is_empty() => {
                self.status = Some("Wait for deletes to finish before rescanning".to_owned());
            }
            KeyCode::Char('r') => self.rescan = true,
            _ => {}
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                if let Mode::Confirm { permanent, targets } =
                    std::mem::replace(&mut self.mode, Mode::Browse)
                {
                    self.start_delete(targets, permanent);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let max = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, max) as usize;
    }

    fn enter_dir(&mut self) {
        if let Some(&idx) = self.rows.get(self.selected)
            && self.tree.nodes[idx].is_dir
        {
            self.cwd = idx;
            self.selected = 0;
            self.refresh_rows();
        }
    }

    fn go_up(&mut self) {
        if let Some(parent) = self.tree.nodes[self.cwd].parent {
            let old = self.cwd;
            self.cwd = parent;
            self.refresh_rows();
            self.selected = self.rows.iter().position(|&i| i == old).unwrap_or(0);
        }
    }

    fn toggle_mark(&mut self) {
        if let Some(&idx) = self.rows.get(self.selected) {
            if !self.marked.remove(&idx) {
                self.marked.insert(idx);
            }
            self.move_selection(1);
        }
    }

    fn request_delete(&mut self, permanent: bool) {
        let mut targets: Vec<usize> = if self.marked.is_empty() {
            self.rows.get(self.selected).copied().into_iter().collect()
        } else {
            self.marked.iter().copied().collect()
        };

        // Drop targets whose ancestor is also targeted — deleting the ancestor
        // removes them anyway, and a second attempt would just fail.
        let set: HashSet<usize> = targets.iter().copied().collect();
        targets.retain(|&t| {
            let mut ancestor = self.tree.nodes[t].parent;
            while let Some(i) = ancestor {
                if set.contains(&i) {
                    return false;
                }
                ancestor = self.tree.nodes[i].parent;
            }
            true
        });

        // Already on its way out in another job.
        targets.retain(|&t| !self.is_deleting(t));

        if targets.is_empty() {
            if !self.jobs.is_empty() {
                self.status = Some("Already being deleted".to_owned());
            }
            return;
        }
        targets.sort_unstable_by(|&a, &b| self.tree.nodes[b].size.cmp(&self.tree.nodes[a].size));
        self.mode = Mode::Confirm { permanent, targets };
    }

    /// True if `idx` or one of its ancestors is a target of a running job.
    pub fn is_deleting(&self, idx: usize) -> bool {
        let mut node = Some(idx);
        while let Some(i) = node {
            if self.jobs.iter().any(|j| j.targets.contains(&i)) {
                return true;
            }
            node = self.tree.nodes[i].parent;
        }
        false
    }

    /// Start deleting `targets` on a background thread.
    pub fn start_delete(&mut self, targets: Vec<usize>, permanent: bool) {
        let items: Vec<(PathBuf, bool, u64)> = targets
            .iter()
            .map(|&t| {
                let n = &self.tree.nodes[t];
                (self.tree.path_of(t), n.is_dir, n.size)
            })
            .collect();
        for t in &targets {
            self.marked.remove(t);
        }
        let done_bytes = Arc::new(AtomicU64::new(0));
        let done = Arc::clone(&done_bytes);
        self.jobs.push(DeleteJob {
            total_bytes: items.iter().map(|i| i.2).sum(),
            label: items[0].0.display().to_string(),
            handle: thread::spawn(move || delete_items(&items, permanent, &done)),
            done_bytes,
            targets,
            permanent,
        });
    }

    /// Apply every finished job to the tree. Called once per frame.
    pub fn poll_jobs(&mut self) {
        let mut i = 0;
        while i < self.jobs.len() {
            if self.jobs[i].handle.is_finished() {
                let job = self.jobs.remove(i);
                let outcomes = job.handle.join().unwrap_or_default();
                self.finish_delete(&job.targets, job.permanent, outcomes);
            } else {
                i += 1;
            }
        }
    }

    fn finish_delete(&mut self, targets: &[usize], permanent: bool, outcomes: Vec<Outcome>) {
        let mut deleted = 0usize;
        let mut freed = 0u64;
        let mut failed = 0usize;
        let mut first_err: Option<String> = None;

        let mut vanished = 0usize;
        let mut partial = false;

        for (&t, outcome) in targets.iter().zip(outcomes) {
            match outcome {
                Outcome::Vanished => vanished += 1,
                Outcome::Deleted => {
                    deleted += 1;
                    freed += self.tree.nodes[t].size;
                }
                Outcome::Failed(e) => {
                    // The target survives, so its recorded size is now
                    // unreliable and so are its ancestors'.
                    failed += 1;
                    if self.tree.nodes[t].is_dir {
                        partial = true;
                    }
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                    continue;
                }
            }
            // Another job may have removed an ancestor first; removing again
            // would subtract this node's size from the ancestors twice.
            if !self.tree.is_gone(t) {
                self.tree.remove(t);
            }
        }

        self.marked.retain(|&m| !self.tree.is_gone(m));
        while self.tree.is_gone(self.cwd) {
            match self.tree.nodes[self.cwd].parent {
                Some(p) => self.cwd = p,
                None => break,
            }
        }
        self.refresh_rows();

        let verb = if permanent {
            "Permanently deleted"
        } else {
            "Moved to Recycle Bin:"
        };
        let mut msg = format!("{verb} {deleted} item(s), {} freed", format_size(freed));
        if vanished > 0 {
            msg.push_str(&format!(" — {vanished} already gone"));
        }
        if failed > 0 {
            msg.push_str(&format!(
                " — {failed} failed ({})",
                first_err.unwrap_or_default()
            ));
        }
        if partial {
            msg.push_str(" — partially deleted, sizes may be stale; press r to rescan");
        }
        self.status = Some(msg);
    }
}

/// Delete each item, adding bytes to `done` as they go. Runs off the UI thread.
fn delete_items(items: &[(PathBuf, bool, u64)], permanent: bool, done: &AtomicU64) -> Vec<Outcome> {
    items
        .iter()
        .map(|(path, is_dir, size)| {
            // The tree can be minutes old on a large scan, so confirm the target
            // still exists rather than reporting a phantom deletion.
            if fs::symlink_metadata(path).is_err() {
                return Outcome::Vanished;
            }

            let result = if permanent {
                if *is_dir {
                    remove_dir_counted(path, done)
                } else {
                    fs::remove_file(path).inspect(|()| {
                        done.fetch_add(*size, Ordering::Relaxed);
                    })
                }
                .map_err(|e| e.to_string())
            } else {
                // The Recycle Bin takes each item in one opaque call, so its
                // progress moves a whole item at a time.
                trash::delete(path)
                    .map_err(explain_trash_error)
                    .inspect(|()| {
                        done.fetch_add(*size, Ordering::Relaxed);
                    })
            };

            match result {
                Ok(()) => Outcome::Deleted,
                // An error does not mean nothing happened: remove_dir_all can
                // delete most of a tree before hitting an open file, and
                // IFileOperation can recycle some items and abort others. If the
                // target is gone, treat it as deleted.
                Err(_) if fs::symlink_metadata(path).is_err() => Outcome::Deleted,
                Err(e) => Outcome::Failed(e),
            }
        })
        .collect()
}

/// `fs::remove_dir_all`, but counting each file's bytes into `done` as it goes.
///
/// Errors on the way down are ignored: the closing `remove_dir_all` retries what
/// is left (read-only files, directory junctions) and its error is the one that
/// counts. Only real directories are entered, never links.
fn remove_dir_counted(dir: &Path, done: &AtomicU64) -> io::Result<()> {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                let _ = remove_dir_counted(&entry.path(), done);
            } else {
                let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if fs::remove_file(entry.path()).is_ok() {
                    done.fetch_add(len, Ordering::Relaxed);
                }
            }
        }
    }
    fs::remove_dir_all(dir)
}

/// Turn the one Recycle Bin failure users actually hit into something actionable.
///
/// `trash` drives `IFileOperation` with `FOF_NO_UI`, so when an item exceeds the
/// Recycle Bin quota Windows aborts the operation and the crate surfaces only
/// "Some operations were aborted", which tells the user nothing.
fn explain_trash_error(e: trash::Error) -> String {
    let text = e.to_string();
    if text.contains("aborted") {
        format!(
            "{text} — usually too large for the Recycle Bin; use D to delete permanently instead"
        )
    } else {
        text
    }
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn format_count_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(1234567), "1,234,567");
    }

    /// Two same-named files in different directories, both marked.
    fn same_name_fixture(tag: &str) -> (std::path::PathBuf, App) {
        use crate::index::{ScanProgress, scan};
        use std::fs::{self, File};
        use std::io::Write as _;

        let root = std::env::temp_dir().join(format!("disktree_del_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("alpha")).unwrap();
        fs::create_dir_all(root.join("beta")).unwrap();
        File::create(root.join("alpha").join("bin"))
            .unwrap()
            .write_all(&[0u8; 300])
            .unwrap();
        File::create(root.join("beta").join("bin"))
            .unwrap()
            .write_all(&[0u8; 400])
            .unwrap();

        let progress = ScanProgress::default();
        let app = App::new(scan(root.clone(), &progress));
        (root, app)
    }

    fn find_node(app: &App, name: &str, parent_name: &str) -> usize {
        (0..app.tree.nodes.len())
            .find(|&i| {
                app.tree.nodes[i].name == name
                    && app.tree.nodes[i]
                        .parent
                        .is_some_and(|p| app.tree.nodes[p].name == parent_name)
            })
            .expect("node not found")
    }

    /// The confirm dialog must distinguish identically named targets, or the user
    /// is agreeing to delete something they cannot identify.
    #[test]
    fn confirm_dialog_shows_full_paths_for_same_named_targets() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (root, mut app) = same_name_fixture("paths");
        let a = find_node(&app, "bin", "alpha");
        let b = find_node(&app, "bin", "beta");
        app.marked.insert(a);
        app.marked.insert(b);
        app.mode = Mode::Confirm {
            permanent: false,
            targets: vec![a, b],
        };

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let screen: String = (0..24)
            .map(|y| {
                (0..100)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            screen.contains("alpha") && screen.contains("beta"),
            "dialog does not distinguish the two targets:\n{screen}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A target deleted out-of-band between scan and confirm must reconcile the
    /// tree rather than report a phantom success or leave a stale size behind.
    #[test]
    fn delete_reconciles_target_that_vanished() {
        let (root, mut app) = same_name_fixture("vanish");
        let a = find_node(&app, "bin", "alpha");
        let root_size_before = app.tree.nodes[app.tree.root].size;

        std::fs::remove_file(app.tree.path_of(a)).unwrap();
        app.start_delete(vec![a], true);
        wait_jobs(&mut app);

        assert!(app.tree.is_gone(a), "vanished target still in tree");
        assert_eq!(
            app.tree.nodes[app.tree.root].size,
            root_size_before - 300,
            "ancestor size not corrected"
        );
        let status = app.status.clone().unwrap_or_default();
        assert!(status.contains("already gone"), "status was {status:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn delete_removes_file_and_updates_ancestors() {
        let (root, mut app) = same_name_fixture("ok");
        let b = find_node(&app, "bin", "beta");
        let before = app.tree.nodes[app.tree.root].size;

        app.start_delete(vec![b], true);
        wait_jobs(&mut app);

        assert!(app.tree.is_gone(b));
        assert_eq!(app.tree.nodes[app.tree.root].size, before - 400);
        assert!(
            !std::path::Path::new(&root)
                .join("beta")
                .join("bin")
                .exists()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn wait_jobs(app: &mut App) {
        while !app.jobs.is_empty() {
            std::thread::yield_now();
            app.poll_jobs();
        }
    }

    /// Two jobs in flight at once, one of them inside the other: both must
    /// finish, and the ancestors must lose each byte exactly once.
    #[test]
    fn concurrent_jobs_including_nested_targets() {
        let (root, mut app) = same_name_fixture("nested");
        let alpha_bin = find_node(&app, "bin", "alpha");
        let alpha = app.tree.nodes[alpha_bin].parent.unwrap();
        let beta_bin = find_node(&app, "bin", "beta");
        let before = app.tree.nodes[app.tree.root].size;

        app.start_delete(vec![alpha_bin], true);
        app.start_delete(vec![alpha, beta_bin], true);
        assert_eq!(app.jobs.len(), 2);
        assert!(app.is_deleting(alpha_bin) && app.is_deleting(beta_bin));
        wait_jobs(&mut app);

        assert!(app.tree.is_gone(alpha) && app.tree.is_gone(beta_bin));
        assert_eq!(app.tree.nodes[app.tree.root].size, before - 700);
        assert!(!root.join("alpha").exists());
        assert!(!root.join("beta").join("bin").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Items already in a running job are not offered for deletion again.
    #[test]
    fn request_skips_targets_already_being_deleted() {
        let (root, mut app) = same_name_fixture("skip");
        let a = find_node(&app, "bin", "alpha");
        let b = find_node(&app, "bin", "beta");
        app.start_delete(vec![a], true);
        app.marked.extend([a, b]);
        app.request_delete(true);
        match &app.mode {
            Mode::Confirm { targets, .. } => assert_eq!(targets, &vec![b]),
            _ => panic!("expected confirm dialog"),
        }
        wait_jobs(&mut app);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The progress counter must reach the bytes actually removed.
    #[test]
    fn remove_dir_counted_reports_bytes() {
        let dir = std::env::temp_dir().join(format!("disktree_count_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a").join("b")).unwrap();
        fs::write(dir.join("top"), [0u8; 100]).unwrap();
        fs::write(dir.join("a").join("mid"), [0u8; 200]).unwrap();
        fs::write(dir.join("a").join("b").join("deep"), [0u8; 300]).unwrap();

        let done = AtomicU64::new(0);
        remove_dir_counted(&dir, &done).unwrap();

        assert!(!dir.exists());
        assert_eq!(done.load(Ordering::Relaxed), 600);
    }
}
