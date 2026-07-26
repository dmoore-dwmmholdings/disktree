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
    let [header_area, table_area, footer_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(2)])
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
        Paragraph::new(text).style(Style::new().add_modifier(Modifier::BOLD).bg(Color::DarkGray)),
        area,
    );
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let total = app.tree.nodes[app.cwd].size.max(1);

    let rows: Vec<Row> = app
        .rows
        .iter()
        .map(|&idx| {
            let node = &app.tree.nodes[idx];
            let marked = app.marked.contains(&idx);
            let pct = node.size as f64 / total as f64 * 100.0;

            let mark_cell = Cell::from(if marked { "●" } else { " " })
                .style(Style::new().fg(Color::Yellow));
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

            let row = Row::new(vec![mark_cell, size_cell, pct_cell, bar_cell, items_cell, name_cell]);
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
        Row::new(vec!["", "      Size", "     %", "", "     Items", "Name"])
            .style(Style::new().add_modifier(Modifier::UNDERLINED).fg(Color::Gray)),
    )
    .row_highlight_style(Style::new().bg(Color::Blue));

    app.table_state.select(if app.rows.is_empty() {
        None
    } else {
        Some(app.selected.min(app.rows.len() - 1))
    });
    f.render_stateful_widget(table, area, &mut app.table_state);

    if app.rows.is_empty() {
        let msg = Paragraph::new("  (empty directory)").style(Style::new().fg(Color::DarkGray));
        let inner = Rect { y: area.y + 1, height: 1, ..area };
        f.render_widget(msg, inner);
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let [info_area, keys_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);

    let info: Line = if let Some(status) = &app.status {
        Line::from(Span::styled(format!(" {status}"), Style::new().fg(Color::Green)))
    } else if !app.marked.is_empty() {
        let marked_size: u64 = app.marked.iter().map(|&i| app.tree.nodes[i].size).sum();
        Line::from(Span::styled(
            format!(" {} marked — {}", app.marked.len(), format_size(marked_size)),
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
        let node = &app.tree.nodes[t];
        lines.push(Line::from(format!(
            "  {}  ({})",
            node.name.to_string_lossy(),
            format_size(node.size)
        )));
    }
    if targets.len() > MAX_SHOWN {
        lines.push(Line::from(format!("  … and {} more", targets.len() - MAX_SHOWN)));
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
    let current = progress.current.lock().map(|c| c.clone()).unwrap_or_default();

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
    use std::fs::{self, File};
    use std::io::Write as _;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

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
        File::create(root.join("big.bin")).unwrap().write_all(&[0u8; 8000]).unwrap();
        File::create(root.join("small.bin")).unwrap().write_all(&[0u8; 2000]).unwrap();

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
}
