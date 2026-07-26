use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table};

use crate::app::{App, Mode, format_count, format_size};
use crate::index::ScanProgress;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const BAR_WIDTH: usize = 10;
const PARTIAL_BLOCKS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header_area, table_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(f.area());

    draw_header(f, app, header_area);
    draw_table(f, app, table_area);
    draw_footer(f, app, footer_area);

    match &app.mode {
        Mode::Confirm { permanent, targets } => draw_confirm(f, app, *permanent, targets),
        Mode::Deleting { count } => draw_deleting(f, *count),
        Mode::Browse => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let node = &app.tree.nodes[app.cwd];
    let mut text = format!(
        " {}  —  {}, {} files",
        app.tree.path_of(app.cwd).display(),
        format_size(node.size),
        format_count(node.n_files),
    );
    if app.tree.errors > 0 {
        text.push_str(&format!("  ({} unreadable)", format_count(app.tree.errors)));
    }
    // Keep the tail (current dir + totals) visible when the path is long.
    let text = truncate_left(&text, area.width as usize);
    f.render_widget(
        Paragraph::new(text).style(
            Style::new()
                .add_modifier(Modifier::BOLD)
                .bg(Color::DarkGray),
        ),
        area,
    );
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let total = app.tree.nodes[app.cwd].size.max(1);
    // One line of the area is the header, so it cannot hold data rows.
    let window = app.visible_range(area.height.saturating_sub(1) as usize);

    let rows: Vec<Row> = app.rows[window.clone()]
        .iter()
        .map(|&idx| {
            let node = &app.tree.nodes[idx];
            let marked = app.marked.contains(&idx);
            let pct = node.size as f64 / total as f64 * 100.0;

            let mark_cell =
                Cell::from(if marked { "●" } else { " " }).style(Style::new().fg(Color::Yellow));
            let size_cell = Cell::from(format!("{:>10}", format_size(node.size)));
            let pct_cell = Cell::from(format!("{pct:>5.1}%"));
            let bar_cell = Cell::from(bar(pct)).style(Style::new().fg(Color::Green));
            let items_cell = Cell::from(if node.is_dir {
                format!("{:>10}", format_count(node.n_files))
            } else {
                String::new()
            });
            let name = node.name.to_string_lossy().into_owned();
            let name_cell = if node.is_dir {
                Cell::from(format!("{name}\\"))
                    .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
            } else {
                Cell::from(name)
            };

            let row = Row::new(vec![
                mark_cell, size_cell, pct_cell, bar_cell, items_cell, name_cell,
            ]);
            if marked {
                row.style(Style::new().fg(Color::Yellow))
            } else {
                row
            }
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(BAR_WIDTH as u16),
            Constraint::Length(10),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new(vec!["", "      Size", "     %", "", "     Items", "Name"]).style(
            Style::new()
                .add_modifier(Modifier::UNDERLINED)
                .fg(Color::Gray),
        ),
    )
    .row_highlight_style(Style::new().bg(Color::Blue));

    // The table only received the visible slice, so the highlight index must be
    // relative to the window, not to the full row list.
    app.table_state.select(if window.is_empty() {
        None
    } else {
        Some(app.selected.saturating_sub(window.start))
    });
    f.render_stateful_widget(table, area, &mut app.table_state);

    if app.rows.is_empty() {
        let msg = Paragraph::new("  (empty directory)").style(Style::new().fg(Color::DarkGray));
        let inner = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        f.render_widget(msg, inner);
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let [info_area, keys_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let info: Line = if let Some(status) = &app.status {
        Line::from(Span::styled(
            format!(" {status}"),
            Style::new().fg(Color::Green),
        ))
    } else if !app.marked.is_empty() {
        let marked_size: u64 = app.marked.iter().map(|&i| app.tree.nodes[i].size).sum();
        Line::from(Span::styled(
            format!(
                " {} marked — {}",
                app.marked.len(),
                format_size(marked_size)
            ),
            Style::new().fg(Color::Yellow),
        ))
    } else {
        Line::default()
    };
    f.render_widget(Paragraph::new(info), info_area);

    let keys = " ↑↓ move  ⏎ open  ⌫ up  space mark  c clear  d → recycle bin  D delete forever  r rescan  q quit";
    f.render_widget(
        Paragraph::new(keys).style(Style::new().fg(Color::DarkGray)),
        keys_area,
    );
}

fn draw_confirm(f: &mut Frame, app: &App, permanent: bool, targets: &[usize]) {
    let total: u64 = targets.iter().map(|&t| app.tree.nodes[t].size).sum();
    let (title, color) = if permanent {
        (" Permanent delete ", Color::Red)
    } else {
        (" Move to Recycle Bin ", Color::Yellow)
    };

    let mut lines: Vec<Line> = Vec::new();
    let question = if permanent {
        format!(
            "PERMANENTLY delete {} item(s) ({})? This cannot be undone.",
            targets.len(),
            format_size(total)
        )
    } else {
        format!(
            "Move {} item(s) ({}) to the Recycle Bin?",
            targets.len(),
            format_size(total)
        )
    };
    lines.push(Line::from(question));
    lines.push(Line::default());

    const MAX_SHOWN: usize = 6;
    for &t in targets.iter().take(MAX_SHOWN) {
        // Full path, not just the name: marks are held across navigation, so a
        // bare name list renders three unrelated "bin" folders as "bin, bin, bin"
        // with nothing to distinguish what is about to be deleted.
        let size = format_size(app.tree.nodes[t].size);
        let path = app.tree.path_of(t).display().to_string();
        let room = 66usize.saturating_sub(size.len() + 6);
        lines.push(Line::from(format!(
            "  {}  ({size})",
            truncate_left(&path, room)
        )));
    }
    if targets.len() > MAX_SHOWN {
        lines.push(Line::from(format!(
            "  … and {} more",
            targets.len() - MAX_SHOWN
        )));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("[Y]", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("es    "),
        Span::styled("[N]", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("o"),
    ]));

    let height = lines.len() as u16 + 2;
    let area = centered(70, height, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(title)
                .border_style(Style::new().fg(color)),
        ),
        area,
    );
}

fn draw_deleting(f: &mut Frame, count: usize) {
    let area = centered(40, 3, f.area());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(format!("Deleting {count} item(s)…"))
            .centered()
            .block(Block::bordered().border_style(Style::new().fg(Color::Yellow))),
        area,
    );
}

pub fn draw_scanning(f: &mut Frame, root: &Path, progress: &ScanProgress, elapsed: Duration) {
    let area = centered(72, 9, f.area());
    let spinner = SPINNER[(elapsed.as_millis() / 80) as usize % SPINNER.len()];

    let dirs = progress.dirs.load(Ordering::Relaxed);
    let files = progress.files.load(Ordering::Relaxed);
    let bytes = progress.bytes.load(Ordering::Relaxed);
    let errors = progress.errors.load(Ordering::Relaxed);
    let current = progress
        .current
        .lock()
        .map(|c| c.clone())
        .unwrap_or_default();

    let max_path = area.width.saturating_sub(14) as usize;
    let shown_path = truncate_left(&current, max_path);

    let mut lines = vec![
        Line::from(format!("{spinner}  Scanning {}", root.display())).bold(),
        Line::default(),
        Line::from(format!(
            "{} folders    {} files    {}",
            format_count(dirs),
            format_count(files),
            format_size(bytes)
        )),
        Line::from(format!("unreadable: {}", format_count(errors)))
            .style(Style::new().fg(Color::DarkGray)),
        Line::from(format!("in: {shown_path}")).style(Style::new().fg(Color::DarkGray)),
        Line::from(format!("elapsed: {:.1} s", elapsed.as_secs_f64())),
        Line::default(),
    ];
    lines.push(Line::from("press q to cancel").style(Style::new().fg(Color::DarkGray)));

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" disktree ")),
        area,
    );
}

fn bar(pct: f64) -> String {
    let eighths = (pct / 100.0 * BAR_WIDTH as f64 * 8.0).round() as usize;
    let full = (eighths / 8).min(BAR_WIDTH);
    let remainder = eighths % 8;
    let mut s = "█".repeat(full);
    if full < BAR_WIDTH && remainder > 0 {
        s.push(PARTIAL_BLOCKS[remainder]);
    }
    while s.chars().count() < BAR_WIDTH {
        s.push(' ');
    }
    s
}

fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_owned();
    }
    let skip = count - max.saturating_sub(1);
    format!("…{}", s.chars().skip(skip).collect::<String>())
}

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{ScanProgress, scan};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::fs::{self, File};
    use std::io::Write as _;

    #[test]
    fn bar_widths() {
        assert_eq!(bar(0.0), "          ");
        assert_eq!(bar(100.0), "██████████");
        assert_eq!(bar(50.0), "█████     ");
    }

    #[test]
    fn table_renders_bars_left_aligned() {
        let root = std::env::temp_dir().join(format!("rustree_ui_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        File::create(root.join("big.bin"))
            .unwrap()
            .write_all(&[0u8; 8000])
            .unwrap();
        File::create(root.join("small.bin"))
            .unwrap()
            .write_all(&[0u8; 2000])
            .unwrap();

        let progress = ScanProgress::default();
        let tree = scan(root.clone(), &progress);
        let mut app = crate::app::App::new(tree);

        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let buffer = terminal.backend().buffer().clone();
        let line = |y: u16| -> String {
            (0..80)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<Vec<_>>()
                .join("")
        };
        // Largest file first, bar left-aligned right after the % column.
        assert!(line(2).contains("80.0% ████████   "), "row: {:?}", line(2));
        assert!(line(2).contains("big.bin"));
        assert!(line(3).contains("20.0% ██        "), "row: {:?}", line(3));
        assert!(line(3).contains("small.bin"));

        fs::remove_dir_all(&root).unwrap();
    }

    /// An `App` over a synthetic tree with `n` equal children, no filesystem.
    fn synthetic_app(n: usize) -> crate::app::App {
        use crate::index::{Node, Tree};
        let mut nodes = vec![Node {
            name: std::ffi::OsString::from("root"),
            is_dir: true,
            size: n as u64 * 10,
            n_files: n as u64,
            parent: None,
            children: (1..=n).collect(),
            deleted: false,
        }];
        for i in 0..n {
            nodes.push(Node {
                name: std::ffi::OsString::from(format!("f{i:06}")),
                is_dir: false,
                // Descending so sort order is predictable and stable.
                size: (n - i) as u64 * 10,
                n_files: 1,
                parent: Some(0),
                children: Vec::new(),
                deleted: false,
            });
        }
        crate::app::App::new(Tree {
            nodes,
            root: 0,
            root_path: std::path::PathBuf::from("T:\\synthetic"),
            errors: 0,
            scan_secs: 0.0,
        })
    }

    /// Rendered data rows, in order, as trimmed name text.
    fn rendered_names(app: &mut crate::app::App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = Vec::new();
        // Row 0 is the header line, then data rows until the footer.
        for y in 2..h.saturating_sub(2) {
            let text: String = (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect();
            if let Some(tok) = text.split_whitespace().find(|t| t.starts_with('f')) {
                out.push(tok.to_owned());
            }
        }
        out
    }

    /// The highlight index is relative to the rendered window, so a scrolled view
    /// must still paint the selected row and no other.
    #[test]
    fn selected_row_is_highlighted_within_the_window() {
        for (n, selected) in [(5usize, 0usize), (5, 3), (1000, 0), (1000, 999)] {
            let mut app = synthetic_app(n);
            app.selected = selected;
            let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            let buf = terminal.backend().buffer().clone();

            // Table occupies y=1 (header) then data rows from y=2.
            let highlighted: Vec<u16> = (2..10)
                .filter(|&y| buf[(2, y)].style().bg == Some(Color::Blue))
                .collect();
            assert_eq!(
                highlighted.len(),
                1,
                "n={n} selected={selected}: {} highlighted rows, expected 1",
                highlighted.len()
            );
            // It must be the row holding the selection.
            let expected_y = 2 + (app.selected - app.offset) as u16;
            assert_eq!(
                highlighted[0], expected_y,
                "n={n} selected={selected}: highlight on wrong row"
            );
        }
    }

    /// The proportional bar is the at-a-glance cue; it must actually be coloured.
    #[test]
    fn bars_render_in_green() {
        let mut app = synthetic_app(4);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // Find any full-block glyph and check its foreground.
        let mut found = false;
        for y in 2..8u16 {
            for x in 0..80u16 {
                if buf[(x, y)].symbol() == "█" {
                    assert_eq!(
                        buf[(x, y)].style().fg,
                        Some(Color::Green),
                        "bar glyph at ({x},{y}) is not green"
                    );
                    found = true;
                }
            }
        }
        assert!(found, "no bar glyphs rendered at all");
    }

    #[test]
    fn window_empty_directory() {
        let mut app = synthetic_app(0);
        assert_eq!(app.visible_range(10), 0..0);
        assert_eq!(app.offset, 0);
        // Must not panic with no rows.
        let _ = rendered_names(&mut app, 80, 12);
    }

    #[test]
    fn window_single_entry() {
        let mut app = synthetic_app(1);
        assert_eq!(app.visible_range(10), 0..1);
        assert_eq!(rendered_names(&mut app, 80, 12), vec!["f000000"]);
    }

    #[test]
    fn window_selection_at_top_shows_first_rows() {
        let mut app = synthetic_app(1000);
        app.selected = 0;
        assert_eq!(app.visible_range(5), 0..5);
        assert_eq!(app.offset, 0);
    }

    #[test]
    fn window_selection_at_end_scrolls_to_bottom() {
        let mut app = synthetic_app(1000);
        app.selected = 999;
        let r = app.visible_range(5);
        assert_eq!(r, 995..1000);
        // The selection must land inside the drawn window.
        assert!(r.contains(&app.selected));
    }

    #[test]
    fn window_shrinks_when_delete_removes_rows() {
        let mut app = synthetic_app(20);
        app.selected = 19;
        app.visible_range(5);
        assert_eq!(app.offset, 15);
        // Simulate the list shrinking under a scrolled-down view.
        app.rows.truncate(6);
        let r = app.visible_range(5);
        assert!(r.end <= 6, "range {r:?} exceeds row count");
        assert!(app.selected < 6, "selection {} out of range", app.selected);
        assert!(r.contains(&app.selected));
    }

    #[test]
    fn window_reclamps_after_resize() {
        let mut app = synthetic_app(100);
        app.selected = 99;
        app.visible_range(5);
        assert_eq!(app.offset, 95);
        // A taller terminal must not leave blank space below the last row.
        let r = app.visible_range(40);
        assert_eq!(r, 60..100);
        assert!(r.contains(&app.selected));
        // And a shorter one must bring the selection back into view.
        let r = app.visible_range(3);
        assert!(r.contains(&app.selected), "range {r:?} lost selection");
    }

    #[test]
    fn window_renders_only_visible_slice() {
        let mut app = synthetic_app(100_000);
        // Sorted by size descending, so f000000 is first.
        let names = rendered_names(&mut app, 80, 12);
        assert!(names.len() <= 12, "drew {} rows into 12 lines", names.len());
        assert_eq!(names.first().map(String::as_str), Some("f000000"));
        assert_eq!(names.get(1).map(String::as_str), Some("f000001"));
    }

    /// The pre-windowing renderer built one widget per child, so a frame cost
    /// grew with directory size (~0.5 s at 100k children). Only the visible slice
    /// is built now, so a 100x larger directory must not cost meaningfully more.
    #[test]
    fn window_render_cost_is_independent_of_directory_size() {
        fn per_frame_secs(n: usize) -> f64 {
            let mut app = synthetic_app(n);
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            // Draw once so lazy setup is not charged to the measurement.
            terminal.draw(|f| draw(f, &mut app)).unwrap();
            let start = std::time::Instant::now();
            for _ in 0..20 {
                terminal.draw(|f| draw(f, &mut app)).unwrap();
            }
            start.elapsed().as_secs_f64() / 20.0
        }

        let small = per_frame_secs(1_000);
        let large = per_frame_secs(100_000);
        eprintln!(
            "MEASURED: {:.3} ms/frame at 1k children, {:.3} ms/frame at 100k",
            small * 1000.0,
            large * 1000.0
        );
        // 100x the children must not cost 5x the frame. Generous, because these
        // are sub-millisecond timings on a shared CI box.
        assert!(
            large < small * 5.0 + 0.002,
            "frame cost scales with directory size: {:.3} ms at 1k vs {:.3} ms at 100k",
            small * 1000.0,
            large * 1000.0
        );
        assert!(large < 0.010, "{:.3} ms per frame at 100k", large * 1000.0);
    }
}
