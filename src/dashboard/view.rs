//! Dashboard geometry: how tall the inline viewport is, which chunks are listed
//! and how the progress band is laid out. Drawing lives further down.

use std::ops::Range;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect, Size};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::dashboard::state::{ChunkRow, ChunkStatus, DashboardState, StageStatus};
use crate::report::Stage;
use crate::run::{MAX_RETRIES, mm_ss};

/// Most chunk rows the list shows; a longer list scrolls.
pub const MAX_CHUNK_ROWS: u16 = 8;
/// Most rows the progress band may span.
pub const MAX_CELL_ROWS: u16 = 3;
/// Narrowest terminal that gets the full frame.
pub const MIN_WIDTH: u16 = 40;

/// Cells of the progress band: size of one cell, gap between cells, how many
/// chunks a cell stands for, and how many rows the band spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellGeometry {
    pub width: u16,
    pub gap: u16,
    pub per_cell: usize,
    pub rows: u16,
}

/// What the viewport shows: the full frame, or a single compact line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Full {
        chunk_rows: u16,
        cells: CellGeometry,
    },
    Compact,
}

impl Layout {
    /// Height of the inline viewport for this layout, in rows.
    pub fn height(&self) -> u16 {
        match self {
            Layout::Compact => 1,
            Layout::Full { chunk_rows, cells } => {
                let list = if *chunk_rows > 0 { 2 + chunk_rows } else { 0 };
                3 + list + 2 + cells.rows + 1
            }
        }
    }
}

/// Geometry of the progress band for `chunks` chunks.
/// `inner_width`: columns inside the progress box (terminal width - 4).
pub fn cell_geometry(chunks: usize, inner_width: u16) -> CellGeometry {
    if chunks == 0 || inner_width == 0 {
        // Nothing to draw (a zero width only happens in the compact layout).
        return CellGeometry {
            width: 1,
            gap: 0,
            per_cell: 1,
            rows: 0,
        };
    }
    let width = usize::from(inner_width);
    if chunks * 5 - 1 <= width {
        return CellGeometry {
            width: 4,
            gap: 1,
            per_cell: 1,
            rows: 1,
        };
    }
    let rows = chunks.div_ceil(width);
    if rows <= usize::from(MAX_CELL_ROWS) {
        // `rows` is at most MAX_CELL_ROWS here.
        return CellGeometry {
            width: 1,
            gap: 0,
            per_cell: 1,
            rows: rows as u16,
        };
    }
    let per_cell = chunks.div_ceil(width * usize::from(MAX_CELL_ROWS));
    let rows = chunks.div_ceil(per_cell).div_ceil(width);
    // By construction `rows` is at most MAX_CELL_ROWS.
    CellGeometry {
        width: 1,
        gap: 0,
        per_cell,
        rows: rows as u16,
    }
}

/// Picks the layout for `chunks` chunks in a terminal of the given size,
/// keeping one free line under the viewport.
pub fn plan_layout(chunks: usize, size: Size) -> Layout {
    if size.width < MIN_WIDTH {
        return Layout::Compact;
    }
    let available = size.height.saturating_sub(1);
    let cells = cell_geometry(chunks, size.width.saturating_sub(4));
    let list_frame = if chunks > 0 { 2 } else { 0 };
    let fixed = 3 + 2 + cells.rows + 1 + list_frame;
    let wanted = u16::try_from(chunks).unwrap_or(u16::MAX);
    let chunk_rows = wanted
        .min(MAX_CHUNK_ROWS)
        .min(available.saturating_sub(fixed));
    if fixed <= available && chunk_rows >= wanted.min(2) {
        Layout::Full { chunk_rows, cells }
    } else {
        Layout::Compact
    }
}

/// Chunks to list and whether the last row summarizes the hidden ones.
pub fn visible_window(
    chunks: usize,
    first_unfinished: Option<usize>,
    rows: u16,
) -> (Range<usize>, bool) {
    let rows = usize::from(rows);
    if chunks <= rows {
        return (0..chunks, false);
    }
    let shown = rows.saturating_sub(1);
    let start = first_unfinished
        .unwrap_or(chunks)
        .saturating_sub(1)
        .min(chunks - shown);
    (start..start + shown, true)
}

/// Spinner frames, one every 100 ms.
pub const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// "0:58", "1:12", "1:02:03" (seconds rounded down).
pub fn fmt_clock(secs: f64) -> String {
    let secs = secs.max(0.0) as u64;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Draws the whole viewport; `now` is the time since the dashboard started.
pub fn render(state: &DashboardState, layout: &Layout, now: Duration, frame: &mut Frame) {
    let spinner = SPINNER[(now.as_millis() / 100 % 10) as usize];
    let area = frame.area();
    match *layout {
        Layout::Compact => {
            frame.render_widget(Paragraph::new(compact_line(state, now, spinner)), area);
        }
        Layout::Full { chunk_rows, cells } => {
            let list_height = if chunk_rows > 0 { 2 + chunk_rows } else { 0 };
            let [header, list, progress] = ratatui::layout::Layout::vertical([
                Constraint::Length(3),
                Constraint::Length(list_height),
                Constraint::Length(2 + cells.rows + 1),
            ])
            .areas(area);
            render_header(state, spinner, header, frame);
            if chunk_rows > 0 {
                render_chunks(state, chunk_rows, now, spinner, list, frame);
            }
            render_progress(state, cells, now, progress, frame);
        }
    }
}

/// A bordered box with a title and one column of padding on each side.
fn boxed(title: String) -> Block<'static> {
    Block::bordered()
        .title(title)
        .padding(Padding::horizontal(1))
}

fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

/// Box 1: input, model and the stage line.
fn render_header(state: &DashboardState, spinner: char, area: Rect, frame: &mut Frame) {
    let mut spans = Vec::new();
    for (i, stage) in Stage::ALL.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        let (icon, color) = match state.stage(stage) {
            StageStatus::Done => ('✔', Color::Green),
            StageStatus::Running => (spinner, Color::Cyan),
            StageStatus::Pending => ('·', Color::DarkGray),
        };
        spans.push(Span::styled(icon.to_string(), fg(color)));
        spans.push(Span::raw(format!(" {}", stage.label())));
    }
    let title = format!(" srtty · {} · {} ", state.input_name, state.model);
    frame.render_widget(Paragraph::new(Line::from(spans)).block(boxed(title)), area);
}

/// Box 2: the visible chunk rows, and a summary row when the list scrolls.
fn render_chunks(
    state: &DashboardState,
    rows: u16,
    now: Duration,
    spinner: char,
    area: Rect,
    frame: &mut Frame,
) {
    let n = state.chunks.len();
    let (window, summary) = visible_window(n, state.first_unfinished(), rows);
    let digits = n.to_string().len();
    let mut lines: Vec<Line> = window
        .clone()
        .map(|i| chunk_line(i, &state.chunks[i], digits, now, spinner))
        .collect();
    if summary {
        lines.push(Line::styled(
            format!("… {} above, {} below", window.start, n - window.end),
            fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(lines).block(boxed(" Chunks ".into())), area);
}

/// `#4 03:00–04:00  ↻ retry 1/3  HTTP 429, retrying in 1 s`
fn chunk_line(
    index: usize,
    row: &ChunkRow,
    digits: usize,
    now: Duration,
    spinner: char,
) -> Line<'static> {
    let head = format!(
        "#{:<digits$} {}–{}  ",
        index + 1,
        mm_ss(row.start_s),
        mm_ss(row.end_s)
    );
    let (status, color) = match &row.status {
        ChunkStatus::Pending => ("· pending".to_string(), Color::DarkGray),
        ChunkStatus::Running => (format!("{spinner} transcribing"), Color::Cyan),
        ChunkStatus::Retry {
            attempt,
            wait_s,
            reason,
            at,
        } => {
            let left = at.as_secs_f64() + wait_s - now.as_secs_f64();
            let wait = if left > 0.0 {
                format!("{reason}, retrying in {} s", left.ceil() as u64)
            } else {
                format!("{reason}, retrying")
            };
            (
                format!("↻ retry {attempt}/{MAX_RETRIES}  {wait}"),
                Color::Yellow,
            )
        }
        ChunkStatus::Done {
            words,
            secs,
            cached,
        } => {
            let tail = if *cached {
                "cached".to_string()
            } else {
                format!("{secs:.1} s")
            };
            (format!("✔ {words} words  {tail}"), Color::Green)
        }
    };
    Line::from(vec![Span::raw(head), Span::styled(status, fg(color))])
}

/// Box 3: the progress band, then the stats line.
fn render_progress(
    state: &DashboardState,
    cells: CellGeometry,
    now: Duration,
    area: Rect,
    frame: &mut Frame,
) {
    let block = boxed(" Progress ".into());
    let mut lines = band_lines(state, cells, block.inner(area).width);
    let mut stats = stats_parts(state, now);
    stats.push(format!("elapsed {}", fmt_clock(now.as_secs_f64())));
    lines.push(Line::raw(stats.join(" · ")));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Rows of the progress band, `cells.rows` at most.
fn band_lines(state: &DashboardState, cells: CellGeometry, width: u16) -> Vec<Line<'static>> {
    let looks: Vec<(char, Color)> = state
        .chunks
        .chunks(cells.per_cell.max(1))
        .map(cell_look)
        .collect();
    let per_row = usize::from((width + cells.gap) / (cells.width + cells.gap)).max(1);
    let gap = " ".repeat(usize::from(cells.gap));
    looks
        .chunks(per_row)
        .take(usize::from(cells.rows))
        .map(|row| {
            let mut spans = Vec::new();
            for (k, &(glyph, color)) in row.iter().enumerate() {
                if k > 0 && !gap.is_empty() {
                    spans.push(Span::raw(gap.clone()));
                }
                let text = glyph.to_string().repeat(usize::from(cells.width));
                spans.push(Span::styled(text, fg(color)));
            }
            Line::from(spans)
        })
        .collect()
}

/// Glyph and color of a cell: the least advanced status of its chunks
/// (retry, then running, then pending, then done).
fn cell_look(group: &[ChunkRow]) -> (char, Color) {
    let rank = |status: &ChunkStatus| match status {
        ChunkStatus::Retry { .. } => 0,
        ChunkStatus::Running => 1,
        ChunkStatus::Pending => 2,
        ChunkStatus::Done { .. } => 3,
    };
    match group.iter().map(|c| &c.status).min_by_key(|s| rank(s)) {
        Some(ChunkStatus::Retry { .. }) => ('▒', Color::Yellow),
        Some(ChunkStatus::Running) => ('▓', Color::Cyan),
        Some(ChunkStatus::Pending) | None => ('░', Color::DarkGray),
        Some(ChunkStatus::Done { .. }) => ('█', Color::Green),
    }
}

/// Known stats: chunk count (once planned), speed and cost.
fn stats_parts(state: &DashboardState, now: Duration) -> Vec<String> {
    let mut parts = Vec::new();
    let n = state.chunks.len();
    if n > 0 {
        parts.push(format!("{}/{n} chunks", state.done_count()));
    }
    if let Some(speed) = state.speed(now) {
        parts.push(format!("{speed:.1}× realtime"));
    }
    if let Some(cost) = state.cost_usd {
        parts.push(format!("${cost:.4}"));
    }
    parts
}

/// `⠋ Transcribe · 5/8 chunks · 4.0× realtime · $0.0200 · 1:10`
fn compact_line(state: &DashboardState, now: Duration, spinner: char) -> String {
    let mut parts: Vec<String> = state
        .current_stage()
        .map(|stage| stage.label().to_string())
        .into_iter()
        .collect();
    parts.extend(stats_parts(state, now));
    parts.push(fmt_clock(now.as_secs_f64()));
    format!("{spinner} {}", parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::report::Event;

    fn size(w: u16, h: u16) -> Size {
        Size::new(w, h)
    }

    const fn s(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    fn plan(n: usize) -> Event {
        Event::ChunksPlanned(
            (0..n)
                .map(|i| (i as f64 * 60.0, (i + 1) as f64 * 60.0))
                .collect(),
        )
    }

    fn done(index: usize, cached: bool) -> Event {
        Event::ChunkDone {
            index,
            words: 100,
            secs: 4.0,
            cost_usd: Some(0.005),
            cached,
        }
    }

    fn stage_done(stage: Stage) -> Event {
        Event::StageDone {
            stage,
            detail: String::new(),
        }
    }

    fn state_with_plan(n: usize) -> DashboardState {
        let mut st = DashboardState::new("talk.mkv".into(), "x-ai/grok-stt-1.0".into());
        st.apply(&plan(n), s(0));
        st
    }

    /// The validated mockup: 8 chunks, one retrying, one running, one pending.
    fn mockup_state() -> DashboardState {
        let mut st = DashboardState::new("talk.mkv".into(), "x-ai/grok-stt-1.0".into());
        st.apply(&Event::StageStarted(Stage::Extract), s(0));
        st.apply(&stage_done(Stage::Extract), s(2));
        st.apply(&Event::StageStarted(Stage::Split), s(2));
        st.apply(&plan(8), s(3));
        st.apply(&stage_done(Stage::Split), s(3));
        st.apply(&Event::StageStarted(Stage::Transcribe), s(10));
        for i in [0, 1, 2, 4] {
            st.apply(&Event::ChunkStarted(i), s(10));
            st.apply(&done(i, false), s(60));
        }
        st.apply(&done(5, true), s(10));
        st.apply(&Event::ChunkStarted(3), s(60));
        st.apply(
            &Event::ChunkRetry {
                index: 3,
                attempt: 1,
                wait_s: 2.0,
                reason: "HTTP 429".into(),
            },
            s(69),
        );
        st.apply(&Event::ChunkStarted(6), s(60));
        st
    }

    /// 20 chunks: the first ten done, the next two running.
    fn state_twenty() -> DashboardState {
        let mut st = state_with_plan(20);
        for i in 0..10 {
            st.apply(&done(i, false), s(1));
        }
        st.apply(&Event::ChunkStarted(10), s(1));
        st.apply(&Event::ChunkStarted(11), s(1));
        st
    }

    /// Renders `state` in a `w` x `h` test terminal and returns its lines.
    fn draw(state: &DashboardState, w: u16, h: u16, now: Duration) -> Vec<String> {
        let layout = plan_layout(state.chunks.len(), Size::new(w, h + 1));
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|frame| render(state, &layout, now, frame))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(w))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    #[test]
    fn renders_the_mockup_layout() {
        let lines = draw(&mockup_state(), 80, 17, s(70)).join("\n");
        assert!(lines.contains("┌ srtty · talk.mkv · x-ai/grok-stt-1.0 "));
        assert!(lines.contains("✔ Extract   ✔ Split   "));
        assert!(lines.contains("Transcribe   · Assemble   · Write"));
        assert!(lines.contains("┌ Chunks "));
        assert!(lines.contains("#1 00:00–01:00  ✔ 100 words"));
        assert!(lines.contains("4.0 s"));
        assert!(lines.contains("#4 03:00–04:00  ↻ retry 1/3"));
        assert!(lines.contains("HTTP 429, retrying in 1 s"));
        assert!(lines.contains("cached"));
        assert!(lines.contains("transcribing"));
        assert!(lines.contains("#8 07:00–08:00  · pending"));
        assert!(lines.contains("████ ████ ████ ▒▒▒▒ ████ ████ ▓▓▓▓ ░░░░"));
        assert!(lines.contains("5/8 chunks · 4.0× realtime · $0.0200 · elapsed 1:10"));
    }

    #[test]
    fn retry_countdown_never_goes_negative() {
        let lines = draw(&mockup_state(), 80, 17, s(75)).join("\n");
        assert!(lines.contains("HTTP 429, retrying"));
        assert!(!lines.contains("retrying in"));
    }

    #[test]
    fn speakers_run_has_one_chunk_row() {
        // plan(1) only: one "#1 " row and one cell
        let lines = draw(&state_with_plan(1), 80, 10, s(1));
        assert_eq!(lines.iter().filter(|l| l.contains("#1 ")).count(), 1);
        assert!(!lines.iter().any(|l| l.contains("#2 ")));
    }

    #[test]
    fn long_lists_scroll_with_a_summary_row() {
        // plan(20), chunks 0..10 done, 10..12 running: window 9..16
        let lines = draw(&state_twenty(), 80, 17, s(1)).join("\n");
        assert!(lines.contains("#10 "));
        assert!(!lines.contains("#9 "));
        assert!(lines.contains("… 9 above, 4 below"));
    }

    #[test]
    fn before_the_plan_only_elapsed_is_shown() {
        let lines = draw(
            &DashboardState::new("a.mp3".into(), "m".into()),
            80,
            6,
            s(3),
        )
        .join("\n");
        assert!(lines.contains("elapsed 0:03"));
        assert!(!lines.contains("chunks ·"));
    }

    #[test]
    fn compact_line() {
        let lines = draw(&mockup_state(), 80, 1, s(70));
        // plan_layout(8, 80x2) is Compact
        assert!(lines[0].contains("Transcribe · 5/8 chunks · 4.0× realtime · $0.0200 · 1:10"));
    }

    #[test]
    fn clock_format() {
        assert_eq!(fmt_clock(3.9), "0:03");
        assert_eq!(fmt_clock(72.4), "1:12");
        assert_eq!(fmt_clock(3723.0), "1:02:03");
    }

    #[test]
    fn cell_geometry_cases() {
        assert_eq!(cell_geometry(0, 76).rows, 0);
        assert_eq!(
            cell_geometry(8, 66),
            CellGeometry {
                width: 4,
                gap: 1,
                per_cell: 1,
                rows: 1
            }
        );
        assert_eq!(
            cell_geometry(20, 76),
            CellGeometry {
                width: 1,
                gap: 0,
                per_cell: 1,
                rows: 1
            }
        );
        assert_eq!(
            cell_geometry(200, 76),
            CellGeometry {
                width: 1,
                gap: 0,
                per_cell: 1,
                rows: 3
            }
        );
        assert_eq!(
            cell_geometry(500, 76),
            CellGeometry {
                width: 1,
                gap: 0,
                per_cell: 3,
                rows: 3
            }
        );
    }

    #[test]
    fn full_layout_heights() {
        assert_eq!(plan_layout(0, size(80, 40)).height(), 6); // before the chunk plan
        assert_eq!(plan_layout(1, size(80, 40)).height(), 10); // --speakers: one chunk row
        assert_eq!(plan_layout(8, size(80, 40)).height(), 17);
        assert!(matches!(
            plan_layout(20, size(80, 40)),
            Layout::Full { chunk_rows: 8, .. }
        ));
        assert!(matches!(
            plan_layout(20, size(80, 14)),
            Layout::Full { chunk_rows: 4, .. }
        ));
    }

    #[test]
    fn layout_degrades_to_compact() {
        assert_eq!(plan_layout(20, size(80, 10)), Layout::Compact);
        assert_eq!(plan_layout(8, size(39, 40)), Layout::Compact);
        assert_eq!(plan_layout(0, size(80, 5)), Layout::Compact);
        assert_eq!(Layout::Compact.height(), 1);
    }

    #[test]
    fn visible_window_follows_first_unfinished() {
        assert_eq!(visible_window(8, Some(3), 8), (0..8, false));
        assert_eq!(visible_window(20, Some(10), 8), (9..16, true));
        assert_eq!(visible_window(20, Some(0), 8), (0..7, true));
        assert_eq!(visible_window(20, Some(19), 8), (13..20, true));
        assert_eq!(visible_window(20, None, 8), (13..20, true));
    }
}
