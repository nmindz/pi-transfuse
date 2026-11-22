//! Multi-select session picker for archive subcommands (pack, unpack, push, pull).
//!
//! Callers pass [`Spec`] rows; this module handles filter, age cutoff, sort, and confirm.

use std::cmp::Ordering;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::tui::{self, ACCENT, DIM, OK, WARN};
use crate::{clip, human_size};

/// One row the list can show and return after confirm.
pub struct Row {
    /// List cells; length matches `Spec::headers`.
    pub columns: Vec<String>,
    /// Project directory; `p` toggles every shown row in the group.
    pub group: String,
    /// Last-used age in days; `None` ignores the threshold filter.
    pub age_days: Option<f64>,
    pub bytes: u64,
    pub details: Vec<(String, String)>,
    pub search: String,
    /// Dimmed and not selectable when set.
    pub blocked: Option<String>,
}

/// Inputs for one picker run.
pub struct Spec {
    pub title: String,
    pub verb: String,
    pub headers: Vec<String>,
    pub rows: Vec<Row>,
    /// `None` hides the unused-age cutoff and `[` / `]` keys.
    pub threshold_days: Option<u32>,
}

// Layout defaults and the unused-age preset ladder.
const THRESHOLD_PRESETS: [u32; 8] = [0, 7, 15, 30, 60, 90, 180, 365];
const COLUMN_CAP: usize = 32;
const STACK_WIDTH: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    OldestFirst,
    LargestFirst,
    ByGroup,
}

impl SortMode {
    fn label(self) -> &'static str {
        match self {
            SortMode::OldestFirst => "oldest first",
            SortMode::LargestFirst => "largest first",
            SortMode::ByGroup => "by group",
        }
    }

    fn next(self) -> Self {
        match self {
            SortMode::OldestFirst => SortMode::LargestFirst,
            SortMode::LargestFirst => SortMode::ByGroup,
            SortMode::ByGroup => SortMode::OldestFirst,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Help,
    Confirm,
}

// In-memory picker; draw only reads this.
struct Picker {
    title: String,
    verb: String,
    headers: Vec<String>,
    rows: Vec<Row>,
    threshold_enabled: bool,
    threshold: u32,
    sort: SortMode,
    selected: Vec<bool>,
    /// Indices into `rows` in display order.
    view: Vec<usize>,
    cursor: usize,
    filter: String,
    editing_filter: bool,
    overlay: Overlay,
    status: String,
    status_color: Color,
    cancelled: bool,
    confirmed: Option<Vec<usize>>,
}

impl Picker {
    fn new(spec: Spec) -> Self {
        let n = spec.rows.len();
        let threshold_enabled = spec.threshold_days.is_some();
        let threshold = spec.threshold_days.unwrap_or(0);
        let mut picker = Self {
            title: spec.title,
            verb: spec.verb,
            headers: spec.headers,
            rows: spec.rows,
            threshold_enabled,
            threshold,
            sort: SortMode::OldestFirst,
            selected: vec![false; n],
            view: Vec::new(),
            cursor: 0,
            filter: String::new(),
            editing_filter: false,
            overlay: Overlay::None,
            status: String::new(),
            status_color: DIM,
            cancelled: false,
            confirmed: None,
        };
        picker.reindex();
        picker
    }

    /// Filter, threshold, sort, then keep the cursor on the same row when possible.
    fn reindex(&mut self) {
        let mut indices: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.passes_filter(i) && self.passes_threshold(i))
            .collect();
        self.sort_indices(&mut indices);
        let old = self.view.get(self.cursor).copied();
        self.view = indices;
        if self.view.is_empty() {
            self.cursor = 0;
        } else if let Some(idx) = old {
            if let Some(pos) = self.view.iter().position(|&r| r == idx) {
                self.cursor = pos;
            } else {
                self.cursor = self.cursor.min(self.view.len() - 1);
            }
        } else {
            self.cursor = self.cursor.min(self.view.len().saturating_sub(1));
        }
    }

    fn passes_filter(&self, index: usize) -> bool {
        if self.filter.is_empty() {
            return true;
        }
        let needle = self.filter.to_lowercase();
        self.rows[index].search.to_lowercase().contains(&needle)
    }

    fn passes_threshold(&self, index: usize) -> bool {
        if !self.threshold_enabled {
            return true;
        }
        match self.rows[index].age_days {
            None => true,
            Some(age) => age >= f64::from(self.threshold),
        }
    }

    fn shown_set(&self) -> std::collections::HashSet<usize> {
        self.view.iter().copied().collect()
    }

    fn sort_indices(&self, indices: &mut [usize]) {
        indices.sort_by(|&a, &b| self.cmp_rows(a, b));
    }

    fn cmp_rows(&self, a: usize, b: usize) -> Ordering {
        let ra = &self.rows[a];
        let rb = &self.rows[b];
        match self.sort {
            SortMode::OldestFirst => {
                let oa = ra.age_days;
                let ob = rb.age_days;
                match (oa, ob) {
                    (None, None) => a.cmp(&b),
                    (None, Some(_)) => Ordering::Greater,
                    (Some(_), None) => Ordering::Less,
                    (Some(aa), Some(ab)) => ab.partial_cmp(&aa).unwrap_or(Ordering::Equal).then(a.cmp(&b)),
                }
            }
            SortMode::LargestFirst => rb.bytes.cmp(&ra.bytes).then(a.cmp(&b)),
            SortMode::ByGroup => ra
                .group
                .cmp(&rb.group)
                .then_with(|| self.cmp_rows_by_age_desc(a, b)),
        }
    }

    fn cmp_rows_by_age_desc(&self, a: usize, b: usize) -> Ordering {
        let ra = &self.rows[a];
        let rb = &self.rows[b];
        match (ra.age_days, rb.age_days) {
            (None, None) => a.cmp(&b),
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(aa), Some(ab)) => ab.partial_cmp(&aa).unwrap_or(Ordering::Equal).then(a.cmp(&b)),
        }
    }

    fn cursor_row_index(&self) -> Option<usize> {
        self.view.get(self.cursor).copied()
    }

    fn is_blocked(&self, index: usize) -> bool {
        self.rows[index].blocked.is_some()
    }

    fn can_select(&self, index: usize) -> bool {
        !self.is_blocked(index)
    }

    fn toggle_at_cursor(&mut self) {
        let Some(index) = self.cursor_row_index() else {
            return;
        };
        if let Some(reason) = &self.rows[index].blocked {
            self.set_status(reason.clone(), WARN);
            return;
        }
        self.selected[index] = !self.selected[index];
    }

    fn select_all_shown(&mut self) {
        let shown = self.shown_set();
        for index in shown {
            if self.can_select(index) {
                self.selected[index] = true;
            }
        }
    }

    fn clear_selection(&mut self) {
        self.selected.fill(false);
    }

    fn toggle_group_at_cursor(&mut self) {
        let Some(index) = self.cursor_row_index() else {
            return;
        };
        let group = self.rows[index].group.clone();
        let targets: Vec<usize> = self
            .view
            .iter()
            .copied()
            .filter(|&i| self.rows[i].group == group && self.can_select(i))
            .collect();
        if targets.is_empty() {
            return;
        }
        let all_selected = targets.iter().all(|&i| self.selected[i]);
        for i in targets {
            self.selected[i] = !all_selected;
        }
    }

    /// `selected ∩ shown`, or the cursor row when that set is empty.
    fn confirm_candidates(&self) -> Vec<usize> {
        let shown = self.shown_set();
        let mut picked: Vec<usize> = self
            .selected
            .iter()
            .enumerate()
            .filter_map(|(i, &sel)| (sel && shown.contains(&i)).then_some(i))
            .collect();
        if picked.is_empty() {
            if let Some(i) = self.cursor_row_index() {
                if self.can_select(i) {
                    picked.push(i);
                }
            }
        }
        picked.sort_unstable();
        picked
    }

    fn selected_count_and_bytes(&self) -> (usize, u64) {
        let mut count = 0usize;
        let mut bytes = 0u64;
        for (i, &sel) in self.selected.iter().enumerate() {
            if sel {
                count += 1;
                bytes += self.rows[i].bytes;
            }
        }
        (count, bytes)
    }

    /// Moves the cutoff to the next preset past the current value.
    fn step_threshold(&mut self, up: bool) {
        if !self.threshold_enabled {
            return;
        }
        let current = self.threshold;
        self.threshold = if up {
            THRESHOLD_PRESETS.iter().copied().find(|&d| d > current).unwrap_or(current)
        } else {
            THRESHOLD_PRESETS.iter().rev().copied().find(|&d| d < current).unwrap_or(0)
        };
        self.reindex();
    }

    fn set_status<S: Into<String>>(&mut self, msg: S, color: Color) {
        self.status = msg.into();
        self.status_color = color;
    }

    fn request_confirm(&mut self) {
        let candidates = self.confirm_candidates();
        if candidates.is_empty() {
            self.set_status("nothing selected to act on", WARN);
            return;
        }
        self.overlay = Overlay::Confirm;
    }

    fn confirm_yes(&mut self) {
        self.confirmed = Some(self.confirm_candidates());
    }
}

/// Confirmed row indices (ascending), or `None` if the user cancelled.
pub fn pick(spec: Spec) -> Result<Option<Vec<usize>>> {
    let mut terminal = ratatui::init();
    let result = run_picker(&mut terminal, spec);
    ratatui::restore();
    result
}

// Key handling; overlays eat keys before list mode sees them.
fn run_picker(terminal: &mut DefaultTerminal, spec: Spec) -> Result<Option<Vec<usize>>> {
    let mut app = Picker::new(spec);
    loop {
        terminal.draw(|frame| draw(frame, &app))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if app.editing_filter {
            match key.code {
                KeyCode::Esc => {
                    app.filter.clear();
                    app.editing_filter = false;
                    app.reindex();
                }
                KeyCode::Enter => app.editing_filter = false,
                KeyCode::Backspace => {
                    app.filter.pop();
                    app.reindex();
                }
                KeyCode::Char(c) => {
                    app.filter.push(c);
                    app.reindex();
                }
                _ => {}
            }
            continue;
        }

        match app.overlay {
            Overlay::Help => {
                app.overlay = Overlay::None;
            }
            Overlay::Confirm => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    app.confirm_yes();
                    break;
                }
                KeyCode::Char('n') | KeyCode::Esc => app.overlay = Overlay::None,
                _ => {}
            },
            Overlay::None => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    app.cancelled = true;
                    break;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.cancelled = true;
                    break;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if app.cursor + 1 < app.view.len() {
                        app.cursor += 1;
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.cursor = app.cursor.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    app.cursor = (app.cursor + 10).min(app.view.len().saturating_sub(1));
                }
                KeyCode::PageUp => app.cursor = app.cursor.saturating_sub(10),
                KeyCode::Char('g') | KeyCode::Home => app.cursor = 0,
                KeyCode::Char('G') | KeyCode::End => {
                    app.cursor = app.view.len().saturating_sub(1);
                }
                KeyCode::Char(' ') => app.toggle_at_cursor(),
                KeyCode::Char('a') => app.select_all_shown(),
                KeyCode::Char('A') => app.clear_selection(),
                KeyCode::Char('p') => app.toggle_group_at_cursor(),
                KeyCode::Char('/') => {
                    app.editing_filter = true;
                    app.filter.clear();
                    app.reindex();
                }
                KeyCode::Char('[') => app.step_threshold(false),
                KeyCode::Char(']') => app.step_threshold(true),
                KeyCode::Char('s') => {
                    app.sort = app.sort.next();
                    app.reindex();
                }
                KeyCode::Enter => app.request_confirm(),
                KeyCode::Char('?') => app.overlay = Overlay::Help,
                _ => {}
            },
        }

        if app.confirmed.is_some() {
            break;
        }
    }

    if app.cancelled {
        return Ok(None);
    }
    Ok(app.confirmed)
}

// Ratatui panes; colors match `crate::tui`.
fn draw(frame: &mut Frame, app: &Picker) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_top(frame, chunks[0], app);

    let stack = area.width < STACK_WIDTH;
    let body = chunks[1];
    if stack {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(body);
        draw_list(frame, split[0], app);
        draw_detail(frame, split[1], app);
    } else {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(body);
        draw_list(frame, split[0], app);
        draw_detail(frame, split[1], app);
    }

    draw_footer(frame, chunks[2], app);
    draw_status(frame, chunks[3], app);

    match app.overlay {
        Overlay::Help => draw_help(frame, area, app),
        Overlay::Confirm => draw_confirm(frame, area, app),
        Overlay::None => {}
    }
}

fn draw_top(frame: &mut Frame, area: Rect, app: &Picker) {
    let mut parts = vec![app.title.clone()];
    if app.threshold_enabled {
        parts.push(format!("unused ≥ {}d", app.threshold));
    }
    parts.push(app.sort.label().to_string());
    if app.editing_filter {
        parts.push(format!("/{}", app.filter));
    } else if !app.filter.is_empty() {
        parts.push(format!("filter: {}", app.filter));
    }
    let title = parts.join(" · ");
    let line = Line::from(Span::styled(format!(" {title}"), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));
    frame.render_widget(Paragraph::new(line), area);
}

// Cap fixed columns; last column eats the remaining width.
fn column_widths(app: &Picker, list_width: u16) -> Vec<usize> {
    let headers = &app.headers;
    let col_count = headers.len().max(1);
    let checkbox = 4usize;
    let usable = list_width.saturating_sub(checkbox as u16) as usize;
    if col_count == 1 {
        return vec![usable];
    }
    let mut widths: Vec<usize> = vec![0; col_count];
    for &row_index in &app.view {
        let row = &app.rows[row_index];
        for (i, cell) in row.columns.iter().enumerate().take(col_count) {
            let w = cell.chars().count().min(COLUMN_CAP);
            widths[i] = widths[i].max(w);
        }
    }
    for (i, header) in headers.iter().enumerate().take(col_count) {
        let w = header.chars().count().min(COLUMN_CAP);
        widths[i] = widths[i].max(w).max(3);
    }
    let fixed: usize = widths.iter().take(col_count - 1).sum::<usize>() + (col_count - 1);
    let last = usable.saturating_sub(fixed).max(8);
    widths[col_count - 1] = last;
    widths
}

fn draw_list(frame: &mut Frame, area: Rect, app: &Picker) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" sessions ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let inner_width = inner.width;
    let widths = column_widths(app, inner_width);

    let header_line = Line::from(
        std::iter::once(Span::styled("    ", Style::default().fg(DIM)))
            .chain(app.headers.iter().enumerate().map(|(i, h)| {
                let w = widths.get(i).copied().unwrap_or(8);
                Span::styled(
                    format!("{:<width$} ", clip(h, w), width = w),
                    Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                )
            }))
            .collect::<Vec<_>>(),
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);
    frame.render_widget(Paragraph::new(header_line), chunks[0]);

    let mut items: Vec<ListItem> = Vec::new();
    if app.rows.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "nothing to choose",
            Style::default().fg(DIM),
        ))));
    } else if app.view.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "no rows match filter / threshold",
            Style::default().fg(DIM),
        ))));
    } else {
        for (pos, &row_index) in app.view.iter().enumerate() {
            let row = &app.rows[row_index];
            let blocked = row.blocked.is_some();
            let selected = app.selected[row_index];
            let prefix = if blocked {
                "·  "
            } else if selected {
                "[x]"
            } else {
                "[ ]"
            };
            let base = if blocked {
                Style::default().fg(DIM)
            } else if pos == app.cursor {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().fg(OK)
            } else {
                Style::default().fg(Color::White)
            };
            let mut spans = vec![Span::styled(format!("{prefix} "), base)];
            for (i, cell) in row.columns.iter().enumerate() {
                let w = widths.get(i).copied().unwrap_or(8);
                spans.push(Span::styled(
                    format!("{:<width$} ", clip(cell, w), width = w),
                    base,
                ));
            }
            items.push(ListItem::new(Line::from(spans)));
        }
    }

    let list = List::new(items);
    let mut state = ListState::default();
    if !app.view.is_empty() {
        state.select(Some(app.cursor));
    }
    frame.render_stateful_widget(list, chunks[1], &mut state);
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &Picker) {
    let lines: Vec<Line> = match app.cursor_row_index() {
        None => vec![Line::from(Span::styled("—", Style::default().fg(DIM)))],
        Some(i) => {
            let details = &app.rows[i].details;
            let width = details.iter().map(|(label, _)| label.chars().count()).max().unwrap_or(0) + 2;
            details
                .iter()
                .flat_map(|(label, value)| {
                    // Multi-line values continue under the value column.
                    value.split('\n').enumerate().map(move |(n, part)| {
                        let label = if n == 0 { label.as_str() } else { "" };
                        Line::from(vec![
                            Span::styled(format!("{label:<width$}"), Style::default().fg(DIM)),
                            Span::raw(part.to_string()),
                        ])
                    })
                })
                .collect()
        }
    };
    let paragraph = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" detail "),
        );
    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &Picker) {
    let (sel_count, sel_bytes) = app.selected_count_and_bytes();
    let text = format!(
        " {} selected · {} │ {} shown of {} │ ? keys ",
        sel_count,
        human_size(sel_bytes),
        app.view.len(),
        app.rows.len()
    );
    let line = Line::from(Span::styled(text, Style::default().fg(DIM)));
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &Picker) {
    let msg = if app.status.is_empty() {
        "j/k move · space toggle · enter confirm · q quit"
    } else {
        &app.status
    };
    let color = if app.status.is_empty() { DIM } else { app.status_color };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(color)))),
        area,
    );
}

fn draw_help(frame: &mut Frame, area: Rect, app: &Picker) {
    let area = tui::centered(area, 70, 78);
    frame.render_widget(Clear, area);
    let mut rows: Vec<(&str, &str)> = vec![
        ("j k ↑ ↓", "move"),
        ("PgDn / PgUp", "page by 10"),
        ("g / G", "first / last"),
        ("space", "toggle row"),
        ("a / A", "select all shown / clear"),
        ("p", "toggle whole group"),
        ("/", "filter"),
        ("s", "cycle sort"),
        ("enter", "confirm selection"),
        ("?", "this help"),
        ("q / esc", "cancel"),
    ];
    if app.threshold_enabled {
        rows.push(("[ / ]", "age threshold presets"));
    }
    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!("  {key:<14}"), Style::default().fg(ACCENT)),
                Span::raw(*what),
            ])
        })
        .collect();
    let paragraph = Paragraph::new(Text::from(lines)).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" keys ")
            .title_alignment(Alignment::Center),
    );
    frame.render_widget(paragraph, area);
}

fn draw_confirm(frame: &mut Frame, area: Rect, app: &Picker) {
    let area = tui::centered(area, 60, 55);
    frame.render_widget(Clear, area);
    let candidates = app.confirm_candidates();
    let total_bytes: u64 = candidates.iter().map(|&i| app.rows[i].bytes).sum();
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "{} {} session(s) · {}?",
                app.verb,
                candidates.len(),
                human_size(total_bytes)
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for &i in candidates.iter().take(10) {
        let row = &app.rows[i];
        let first = row.columns.first().map(String::as_str).unwrap_or("—");
        let last = row.columns.last().map(String::as_str).unwrap_or("");
        lines.push(Line::from(Span::styled(
            format!("  {first} … {last}"),
            Style::default().fg(DIM),
        )));
    }
    if candidates.len() > 10 {
        lines.push(Line::from(Span::styled(
            format!("  … and {} more", candidates.len() - 10),
            Style::default().fg(DIM),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "y / enter confirm · n / esc back",
        Style::default().fg(DIM),
    )));
    let paragraph = Paragraph::new(Text::from(lines))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(format!(" {} ", app.verb))
                .title_alignment(Alignment::Center),
        );
    frame.render_widget(paragraph, area);
}

// Selection/threshold rules only; no terminal.
#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(id: &str, group: &str, age: Option<f64>, bytes: u64, blocked: bool) -> Row {
        Row {
            columns: vec![id.to_string(), group.to_string(), format!("{bytes}")],
            group: group.to_string(),
            age_days: age,
            bytes,
            details: vec![("id".into(), id.into())],
            search: format!("{id} {group}"),
            blocked: if blocked {
                Some("blocked".into())
            } else {
                None
            },
        }
    }

    fn picker_with(rows: Vec<Row>, threshold: Option<u32>) -> Picker {
        Picker::new(Spec {
            title: "test".into(),
            verb: "pack".into(),
            headers: vec!["name".into(), "group".into(), "size".into()],
            rows,
            threshold_days: threshold,
        })
    }

    #[test]
    fn threshold_hides_younger_keeps_none_age() {
        let rows = vec![
            sample_row("young", "/a", Some(5.0), 100, false),
            sample_row("old", "/a", Some(40.0), 200, false),
            sample_row("na", "/a", None, 50, false),
        ];
        let mut p = picker_with(rows, Some(30));
        assert_eq!(p.threshold, 30);
        p.reindex();
        let shown: Vec<_> = p.view.clone();
        assert!(shown.contains(&1));
        assert!(shown.contains(&2));
        assert!(!shown.contains(&0));
    }

    #[test]
    fn confirm_is_selected_intersect_shown() {
        let rows = vec![
            sample_row("a", "/p", Some(50.0), 10, false),
            sample_row("b", "/p", Some(50.0), 20, false),
        ];
        let mut p = picker_with(rows, None);
        p.selected[0] = true;
        p.selected[1] = true;
        p.filter = "b".into();
        p.reindex();
        assert_eq!(p.view, vec![1]);
        let got = p.confirm_candidates();
        assert_eq!(got, vec![1]);
    }

    #[test]
    fn confirm_falls_back_to_cursor_when_none_selected() {
        let rows = vec![
            sample_row("a", "/p", Some(50.0), 10, false),
            sample_row("b", "/p", Some(50.0), 20, false),
        ];
        let mut p = picker_with(rows, None);
        p.cursor = 1;
        let got = p.confirm_candidates();
        assert_eq!(got, vec![1]);
    }

    #[test]
    fn group_toggle_selects_then_deselects_shown_non_blocked() {
        let rows = vec![
            sample_row("a1", "/g1", Some(10.0), 10, false),
            sample_row("a2", "/g1", Some(10.0), 10, false),
            sample_row("b1", "/g2", Some(10.0), 10, false),
        ];
        let mut p = picker_with(rows, None);
        p.reindex();
        p.cursor = 0;
        p.toggle_group_at_cursor();
        assert!(p.selected[0] && p.selected[1]);
        assert!(!p.selected[2]);
        p.toggle_group_at_cursor();
        assert!(!p.selected[0] && !p.selected[1]);
    }

    #[test]
    fn blocked_rows_never_selected_via_space_all_or_group() {
        let rows = vec![
            sample_row("ok", "/g", Some(10.0), 10, false),
            sample_row("no", "/g", Some(10.0), 10, true),
        ];
        let mut p = picker_with(rows, None);
        p.reindex();
        p.cursor = 1;
        p.toggle_at_cursor();
        assert!(!p.selected[1]);
        p.select_all_shown();
        assert!(p.selected[0]);
        assert!(!p.selected[1]);
        p.clear_selection();
        p.cursor = 0;
        p.toggle_group_at_cursor();
        assert!(p.selected[0]);
        assert!(!p.selected[1]);
    }
}