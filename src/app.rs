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

        for &t in targets {
            let path = self.tree.path_of(t);
            let result = if permanent {
                if self.tree.nodes[t].is_dir {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                }
                .map_err(|e| e.to_string())
            } else {
                trash::delete(&path).map_err(|e| e.to_string())
            };

            match result {
                Ok(()) => {
                    deleted += 1;
                    freed += self.tree.nodes[t].size;
                    self.tree.remove(t);
                }
                Err(e) => {
                    failed += 1;
                    if first_err.is_none() {
                        first_err = Some(e);
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
        if failed > 0 {
            msg.push_str(&format!(
                " — {failed} failed ({})",
                first_err.unwrap_or_default()
            ));
        }
        self.status = Some(msg);
        self.mode = Mode::Browse;
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
        // Not `is_multiple_of`: that is stable only since 1.87, above our MSRV.
        if i > 0 && (digits.len() - i) % 3 == 0 {
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
}
