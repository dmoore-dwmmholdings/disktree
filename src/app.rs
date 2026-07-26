use std::collections::HashSet;
use std::fs;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

use crate::index::Tree;

pub enum Mode {
    Browse,
    Confirm {
        permanent: bool,
        targets: Vec<usize>,
    },
    Deleting {
        count: usize,
    },
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
    /// Set when a delete is confirmed; executed by the main loop after one
    /// "Deleting…" frame has been drawn.
    pub pending_delete: Option<(Vec<usize>, bool)>,
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
            pending_delete: None,
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
            Mode::Deleting { .. } => {}
        }
    }

    fn on_key_browse(&mut self, key: KeyEvent) {
        self.status = None;
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true;
            }
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
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
                    self.mode = Mode::Deleting {
                        count: targets.len(),
                    };
                    self.pending_delete = Some((targets, permanent));
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

        if targets.is_empty() {
            return;
        }
        targets.sort_unstable_by(|&a, &b| self.tree.nodes[b].size.cmp(&self.tree.nodes[a].size));
        self.mode = Mode::Confirm { permanent, targets };
    }

    pub fn perform_delete(&mut self, targets: &[usize], permanent: bool) {
        let mut deleted = 0usize;
        let mut freed = 0u64;
        let mut failed = 0usize;
        let mut first_err: Option<String> = None;

        let mut vanished = 0usize;
        let mut partial = false;

        for &t in targets {
            let path = self.tree.path_of(t);

            // The tree can be minutes old on a large scan, so confirm the target
            // still exists rather than reporting a phantom deletion.
            if fs::symlink_metadata(&path).is_err() {
                vanished += 1;
                self.tree.remove(t);
                continue;
            }

            let result = if permanent {
                if self.tree.nodes[t].is_dir {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                }
                .map_err(|e| e.to_string())
            } else {
                trash::delete(&path).map_err(explain_trash_error)
            };

            match result {
                Ok(()) => {
                    deleted += 1;
                    freed += self.tree.nodes[t].size;
                    self.tree.remove(t);
                }
                Err(e) => {
                    // An error does not mean nothing happened: remove_dir_all can
                    // delete most of a tree before hitting an open file, and
                    // IFileOperation can recycle some items and abort others. If
                    // the target is gone, treat it as deleted; if it survives, its
                    // recorded size is now unreliable and so are its ancestors'.
                    if fs::symlink_metadata(&path).is_err() {
                        deleted += 1;
                        freed += self.tree.nodes[t].size;
                        self.tree.remove(t);
                    } else {
                        failed += 1;
                        if self.tree.nodes[t].is_dir {
                            partial = true;
                        }
                        if first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
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
        self.mode = Mode::Browse;
    }
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
        app.perform_delete(&[a], true);

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

        app.perform_delete(&[b], true);

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
}
