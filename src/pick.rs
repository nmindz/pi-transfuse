//! Multi-select session picker for the archive subcommands (pack, unpack, push, pull).
//!
//! Callers pass [`Spec`] rows; this module handles sorting, filtering, the age cutoff, and confirm.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::facts::{Facts, Sort, SortKey};
use crate::tui::{self, ACCENT, DIM, OK, WARN};
use crate::units::age_days;
use crate::{clip, human_size, Side};

/// One row the list can show and return after confirm.
pub struct Row {
    /// List cells; length matches `Spec::headers`.
    pub columns: Vec<String>,
    /// What the row sorts, filters, and groups by.
    pub facts: Facts,
    pub details: Vec<(String, String)>,
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
    pub sort: Sort,
    /// Time ages are measured against.
    pub now_ms: i64,
}

// Layout defaults and the unused-age preset ladder.
const THRESHOLD_PRESETS: [u32; 8] = [0, 7, 15, 30, 60, 90, 180, 365];
const COLUMN_CAP: usize = 32;
const STACK_WIDTH: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Help,
    Confirm,
    Filters,
}

/// One line of the filter panel.
#[derive(Debug, Clone, PartialEq)]
enum Facet {
    Harness(Side),
    Project(String),
}

struct FacetLine {
    facet: Facet,
    sessions: usize,
    bytes: u64,
}

// In-memory picker; draw only reads this.
struct Picker {
    title: String,
    verb: String,
    headers: Vec<String>,
    rows: Vec<Row>,
    now_ms: i64,
    threshold_enabled: bool,
    threshold: u32,
    sort: Sort,
    hidden_harnesses: HashSet<Side>,
    hidden_projects: HashSet<String>,
    /// Harnesses then projects present among the rows, for the filter panel.
    facets: Vec<FacetLine>,
    facet_cursor: usize,
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
        let mut picker = Self {
            title: spec.title,
            verb: spec.verb,
            headers: spec.headers,
            rows: spec.rows,
            now_ms: spec.now_ms,
            threshold_enabled: spec.threshold_days.is_some(),
            threshold: spec.threshold_days.unwrap_or(0),
            sort: spec.sort,
            hidden_harnesses: HashSet::new(),
            hidden_projects: HashSet::new(),
            facets: Vec::new(),
            facet_cursor: 0,
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
    /// The filter panel lists only what the text filter and threshold leave.
    fn reindex(&mut self) {
        let needle = self.filter.to_lowercase();
        let eligible: Vec<usize> = (0..self.rows.len()).filter(|&i| self.eligible(i, &needle)).collect();
        let old_facet = self.facets.get(self.facet_cursor).map(|l| l.facet.clone());
        self.facets = facets(eligible.iter().map(|&i| &self.rows[i]));
        self.facet_cursor = old_facet
            .and_then(|f| self.facets.iter().position(|l| l.facet == f))
            .unwrap_or_else(|| self.facet_cursor.min(self.facets.len().saturating_sub(1)));

        let mut indices: Vec<usize> = eligible.into_iter().filter(|&i| !self.hidden_by_facet(i)).collect();
        indices.sort_by(|&a, &b| self.sort.compare(&self.rows[a].facts, &self.rows[b].facts));
        let old = self.view.get(self.cursor).copied();
        self.view = indices;
        self.cursor = match old.and_then(|idx| self.view.iter().position(|&r| r == idx)) {
            Some(pos) => pos,
            None => self.cursor.min(self.view.len().saturating_sub(1)),
        };
    }

    /// Passes the text filter and the threshold.
    fn eligible(&self, index: usize, needle: &str) -> bool {
        let facts = &self.rows[index].facts;
        facts.mentions(needle)
            && (!self.threshold_enabled || age_days(self.now_ms, facts.last_used_ms) >= f64::from(self.threshold))
    }

    fn hidden_by_facet(&self, index: usize) -> bool {
        let facts = &self.rows[index].facts;
        self.hidden_harnesses.contains(&facts.harness) || self.hidden_projects.contains(&facts.project)
    }

    fn shown_set(&self) -> HashSet<usize> {
        self.view.iter().copied().collect()
    }

    fn cursor_row_index(&self) -> Option<usize> {
        self.view.get(self.cursor).copied()
    }

    fn can_select(&self, index: usize) -> bool {
        self.rows[index].blocked.is_none()
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

    /// Selects every shown, selectable row; when they all are already, deselects them.
    fn toggle_all_shown(&mut self) {
        let targets: Vec<usize> = self.view.iter().copied().filter(|&i| self.can_select(i)).collect();
        let all = !targets.is_empty() && targets.iter().all(|&i| self.selected[i]);
        for i in targets {
            self.selected[i] = !all;
        }
    }

    /// Checkbox for the shown, selectable rows: none, some, or all selected.
    fn shown_check(&self) -> &'static str {
        let targets = self.view.iter().filter(|&&i| self.can_select(i));
        let (total, picked) = targets.fold((0, 0), |(t, p), &i| (t + 1, p + usize::from(self.selected[i])));
        match picked {
            0 => "[ ]",
            n if n == total => "[x]",
            _ => "[-]",
        }
    }

    fn clear_selection(&mut self) {
        self.selected.fill(false);
    }

    /// Toggles every shown, selectable row in the cursor row's directory.
    fn toggle_path_at_cursor(&mut self) {
        let Some(index) = self.cursor_row_index() else {
            return;
        };
        let path = self.rows[index].facts.path.clone();
        let targets: Vec<usize> = self
            .view
            .iter()
            .copied()
            .filter(|&i| self.rows[i].facts.path == path && self.can_select(i))
            .collect();
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
            if let Some(i) = self.cursor_row_index().filter(|&i| self.can_select(i)) {
                picked.push(i);
            }
        }
        picked.sort_unstable();
        picked
    }

    fn selected_count_and_bytes(&self) -> (usize, u64) {
        self.selected
            .iter()
            .enumerate()
            .filter(|(_, &sel)| sel)
            .fold((0, 0), |(n, bytes), (i, _)| (n + 1, bytes + self.rows[i].facts.bytes))
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

    fn sort_by(&mut self, key: SortKey) {
        self.sort = self.sort.toggled(key);
        self.reindex();
        self.set_status(format!("sorted by {}", self.sort.describe()), DIM);
    }

    fn facet_shown(&self, facet: &Facet) -> bool {
        match facet {
            Facet::Harness(side) => !self.hidden_harnesses.contains(side),
            Facet::Project(name) => !self.hidden_projects.contains(name),
        }
    }

    fn toggle_facet(&mut self) {
        let Some(line) = self.facets.get(self.facet_cursor) else { return };
        match &line.facet {
            Facet::Harness(side) => flip(&mut self.hidden_harnesses, *side),
            Facet::Project(name) => flip(&mut self.hidden_projects, name.clone()),
        }
        self.reindex();
    }

    /// Shows only the cursor's facet among its kind: one harness, or one project.
    fn only_facet(&mut self) {
        let Some(line) = self.facets.get(self.facet_cursor) else { return };
        match &line.facet {
            Facet::Harness(keep) => {
                self.hidden_harnesses = self
                    .facets
                    .iter()
                    .filter_map(|l| match l.facet {
                        Facet::Harness(side) if side != *keep => Some(side),
                        _ => None,
                    })
                    .collect();
            }
            Facet::Project(keep) => {
                self.hidden_projects = self
                    .facets
                    .iter()
                    .filter_map(|l| match &l.facet {
                        Facet::Project(name) if name != keep => Some(name.clone()),
                        _ => None,
                    })
                    .collect();
            }
        }
        self.reindex();
    }

    fn show_all_facets(&mut self) {
        self.hidden_harnesses.clear();
        self.hidden_projects.clear();
        self.reindex();
    }

    /// What the filters currently hide, for the top bar.
    fn hiding(&self) -> Option<String> {
        let mut parts: Vec<String> = self
            .facets
            .iter()
            .filter_map(|l| match &l.facet {
                Facet::Harness(side) if self.hidden_harnesses.contains(side) => Some(format!("−{}", side.key())),
                _ => None,
            })
            .collect();
        // Only hidden projects the panel still lists; the rest hide nothing.
        let hidden: Vec<&str> = self
            .facets
            .iter()
            .filter_map(|l| match &l.facet {
                Facet::Project(name) if self.hidden_projects.contains(name) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        match hidden.as_slice() {
            [] => {}
            [one] => parts.push(format!("−{one}")),
            many => parts.push(format!("−{} projects", many.len())),
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    fn set_status<S: Into<String>>(&mut self, msg: S, color: Color) {
        self.status = msg.into();
        self.status_color = color;
    }

    fn request_confirm(&mut self) {
        if self.confirm_candidates().is_empty() {
            self.set_status("nothing selected to act on", WARN);
            return;
        }
        self.overlay = Overlay::Confirm;
    }
}

fn flip<T: Eq + std::hash::Hash>(set: &mut HashSet<T>, item: T) {
    if !set.remove(&item) {
        set.insert(item);
    }
}

/// Harnesses in listing order, then projects largest first, with their totals.
fn facets<'a>(rows: impl Iterator<Item = &'a Row>) -> Vec<FacetLine> {
    let mut harnesses: BTreeMap<&str, (Side, usize, u64)> = BTreeMap::new();
    let mut projects: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    for row in rows {
        let f = &row.facts;
        let h = harnesses.entry(f.harness.key()).or_insert((f.harness, 0, 0));
        h.1 += 1;
        h.2 += f.bytes;
        let p = projects.entry(f.project.as_str()).or_default();
        p.0 += 1;
        p.1 += f.bytes;
    }
    let mut out: Vec<FacetLine> = crate::facts::ALL_SIDES
        .iter()
        .filter_map(|side| harnesses.get(side.key()))
        .map(|&(side, sessions, bytes)| FacetLine { facet: Facet::Harness(side), sessions, bytes })
        .collect();
    let mut projects: Vec<FacetLine> = projects
        .into_iter()
        .map(|(name, (sessions, bytes))| FacetLine { facet: Facet::Project(name.to_string()), sessions, bytes })
        .collect();
    projects.sort_by_key(|l| std::cmp::Reverse(l.bytes));
    out.extend(projects);
    out
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
    while !app.cancelled && app.confirmed.is_none() {
        terminal.draw(|frame| draw(frame, &app))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            app.cancelled = true;
        } else if app.editing_filter {
            filter_key(&mut app, key.code);
        } else {
            match app.overlay {
                Overlay::Help => app.overlay = Overlay::None,
                Overlay::Confirm => match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => app.confirmed = Some(app.confirm_candidates()),
                    KeyCode::Char('n') | KeyCode::Esc => app.overlay = Overlay::None,
                    _ => {}
                },
                Overlay::Filters => facet_key(&mut app, key.code),
                Overlay::None => list_key(&mut app, key.code),
            }
        }
    }
    Ok(if app.cancelled { None } else { app.confirmed })
}

fn filter_key(app: &mut Picker, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            app.filter.clear();
            app.editing_filter = false;
        }
        KeyCode::Enter => app.editing_filter = false,
        KeyCode::Backspace => {
            app.filter.pop();
        }
        KeyCode::Char(c) => app.filter.push(c),
        _ => return,
    }
    app.reindex();
}

fn facet_key(app: &mut Picker, code: KeyCode) {
    match code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('f' | 'q') => app.overlay = Overlay::None,
        KeyCode::Char('j') | KeyCode::Down => {
            app.facet_cursor = (app.facet_cursor + 1).min(app.facets.len().saturating_sub(1));
        }
        KeyCode::Char('k') | KeyCode::Up => app.facet_cursor = app.facet_cursor.saturating_sub(1),
        KeyCode::Char('g') | KeyCode::Home => app.facet_cursor = 0,
        KeyCode::Char('G') | KeyCode::End => app.facet_cursor = app.facets.len().saturating_sub(1),
        KeyCode::Char(' ') => app.toggle_facet(),
        KeyCode::Char('o') => app.only_facet(),
        KeyCode::Char('a') => app.show_all_facets(),
        _ => {}
    }
}

fn list_key(app: &mut Picker, code: KeyCode) {
    let last = app.view.len().saturating_sub(1);
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.cancelled = true,
        KeyCode::Char('j') | KeyCode::Down => app.cursor = (app.cursor + 1).min(last),
        KeyCode::Char('k') | KeyCode::Up => app.cursor = app.cursor.saturating_sub(1),
        KeyCode::PageDown => app.cursor = (app.cursor + 10).min(last),
        KeyCode::PageUp => app.cursor = app.cursor.saturating_sub(10),
        KeyCode::Char('g') | KeyCode::Home => app.cursor = 0,
        KeyCode::Char('G') | KeyCode::End => app.cursor = last,
        KeyCode::Char(' ') => app.toggle_at_cursor(),
        KeyCode::Char('a') => app.toggle_all_shown(),
        KeyCode::Char('A') => app.clear_selection(),
        KeyCode::Char('p') => app.toggle_path_at_cursor(),
        KeyCode::Char('/') => {
            app.editing_filter = true;
            app.filter.clear();
            app.reindex();
        }
        KeyCode::Char('f') => app.overlay = Overlay::Filters,
        KeyCode::Char('[') => app.step_threshold(false),
        KeyCode::Char(']') => app.step_threshold(true),
        KeyCode::Enter => app.request_confirm(),
        KeyCode::Char('?') => app.overlay = Overlay::Help,
        KeyCode::Char(c) => {
            if let Some(key) = SortKey::ALL.into_iter().find(|k| k.key() == c) {
                app.sort_by(key);
            }
        }
        _ => {}
    }
}

// Ratatui panes; colors match `crate::tui`.
fn draw(frame: &mut Frame, app: &Picker) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(6), Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    draw_top(frame, chunks[0], app);

    let (direction, sizes) = if area.width < STACK_WIDTH {
        (Direction::Vertical, [Constraint::Percentage(55), Constraint::Percentage(45)])
    } else {
        (Direction::Horizontal, [Constraint::Percentage(60), Constraint::Percentage(40)])
    };
    let split = Layout::default().direction(direction).constraints(sizes).split(chunks[1]);
    draw_list(frame, split[0], app);
    draw_detail(frame, split[1], app);

    draw_footer(frame, chunks[2], app);
    draw_status(frame, chunks[3], app);

    match app.overlay {
        Overlay::Help => draw_help(frame, area, app),
        Overlay::Confirm => draw_confirm(frame, area, app),
        Overlay::Filters => draw_filters(frame, area, app),
        Overlay::None => {}
    }
}

fn draw_top(frame: &mut Frame, area: Rect, app: &Picker) {
    let mut parts = vec![app.title.clone()];
    if app.threshold_enabled {
        parts.push(format!("unused ≥ {}d", app.threshold));
    }
    parts.push(app.sort.describe());
    if let Some(hiding) = app.hiding() {
        parts.push(hiding);
    }
    if app.editing_filter {
        parts.push(format!("/{}", app.filter));
    } else if !app.filter.is_empty() {
        parts.push(format!("filter: {}", app.filter));
    }
    let title = parts.join(" · ");
    let line = Line::from(Span::styled(format!(" {title}"), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));
    frame.render_widget(Paragraph::new(line), area);
}

/// First visible line that keeps `cursor` in the middle of `height` lines,
/// pinned to the top or bottom when the list cannot scroll that far.
fn centered_offset(cursor: usize, len: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    cursor.saturating_sub(height / 2).min(len - height)
}

// Cap fixed columns; last column eats the remaining width.
fn column_widths(app: &Picker, list_width: u16) -> Vec<usize> {
    let col_count = app.headers.len().max(1);
    let usable = list_width.saturating_sub(4) as usize;
    if col_count == 1 {
        return vec![usable];
    }
    let mut widths: Vec<usize> = vec![0; col_count];
    for &row_index in &app.view {
        for (i, cell) in app.rows[row_index].columns.iter().enumerate().take(col_count) {
            widths[i] = widths[i].max(cell.chars().count().min(COLUMN_CAP));
        }
    }
    for (i, header) in app.headers.iter().enumerate().take(col_count) {
        widths[i] = widths[i].max(header.chars().count().min(COLUMN_CAP)).max(3);
    }
    let fixed: usize = widths.iter().take(col_count - 1).sum::<usize>() + (col_count - 1);
    widths[col_count - 1] = usable.saturating_sub(fixed).max(8);
    widths
}

fn draw_list(frame: &mut Frame, area: Rect, app: &Picker) {
    let block = Block::bordered().border_type(BorderType::Rounded).title(" sessions ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let widths = column_widths(app, inner.width);

    let header_line = Line::from(
        std::iter::once(Span::styled(format!("{} ", app.shown_check()), Style::default().fg(DIM)))
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

    let placeholder = |text: &'static str| vec![ListItem::new(Line::from(Span::styled(text, Style::default().fg(DIM))))];
    let items: Vec<ListItem> = if app.rows.is_empty() {
        placeholder("nothing to choose")
    } else if app.view.is_empty() {
        placeholder("no rows match the filters or threshold")
    } else {
        app.view
            .iter()
            .enumerate()
            .map(|(pos, &row_index)| {
                let row = &app.rows[row_index];
                let selected = app.selected[row_index];
                let (prefix, base) = if row.blocked.is_some() {
                    ("·  ", Style::default().fg(DIM))
                } else if pos == app.cursor {
                    (if selected { "[x]" } else { "[ ]" }, Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
                } else if selected {
                    ("[x]", Style::default().fg(OK))
                } else {
                    ("[ ]", Style::default().fg(Color::White))
                };
                let mut spans = vec![Span::styled(format!("{prefix} "), base)];
                for (i, cell) in row.columns.iter().enumerate() {
                    let w = widths.get(i).copied().unwrap_or(8);
                    spans.push(Span::styled(format!("{:<width$} ", clip(cell, w), width = w), base));
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    };

    let height = usize::from(chunks[1].height);
    let mut state = ListState::default().with_offset(centered_offset(app.cursor, app.view.len(), height));
    if !app.view.is_empty() {
        state.select(Some(app.cursor));
    }
    frame.render_stateful_widget(List::new(items), chunks[1], &mut state);
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
        .block(Block::bordered().border_type(BorderType::Rounded).title(" detail "));
    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &Picker) {
    let (sel_count, sel_bytes) = app.selected_count_and_bytes();
    let text = format!(
        " {} selected · {} │ {} shown of {} │ f filters · ? keys ",
        sel_count,
        human_size(sel_bytes),
        app.view.len(),
        app.rows.len()
    );
    frame.render_widget(Paragraph::new(Line::from(Span::styled(text, Style::default().fg(DIM)))), area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &Picker) {
    let (msg, color) = if app.status.is_empty() {
        ("j/k move · space toggle · a all · / search · f filters · u s n d sort · enter confirm · q quit", DIM)
    } else {
        (app.status.as_str(), app.status_color)
    };
    frame.render_widget(Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(color)))), area);
}

fn draw_help(frame: &mut Frame, area: Rect, app: &Picker) {
    let area = tui::centered(area, 70, 85);
    frame.render_widget(Clear, area);
    let mut rows: Vec<(String, String)> = [
        ("j k ↑ ↓", "move"),
        ("PgDn / PgUp", "page by 10"),
        ("g / G", "first / last"),
        ("space", "toggle row"),
        ("a / A", "select or deselect every shown row / clear all"),
        ("p", "toggle every shown row in this path"),
        ("/", "filter by id, title, or path"),
        ("f", "show or hide harnesses and projects"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    if app.threshold_enabled {
        rows.push(("[ / ]".into(), "unused-for threshold presets".into()));
    }
    for key in SortKey::ALL {
        rows.push((key.key().to_string(), format!("sort by {} (again to reverse)", key.label())));
    }
    rows.extend([("enter".into(), "confirm selection".into()), ("? / q".into(), "this help / cancel".into())]);
    let lines: Vec<Line> = rows
        .iter()
        .map(|(key, what)| {
            Line::from(vec![Span::styled(format!("  {key:<14}"), Style::default().fg(ACCENT)), Span::raw(what.clone())])
        })
        .collect();
    let paragraph = Paragraph::new(Text::from(lines)).block(
        Block::bordered().border_type(BorderType::Rounded).title(" keys ").title_alignment(Alignment::Center),
    );
    frame.render_widget(paragraph, area);
}

fn draw_filters(frame: &mut Frame, area: Rect, app: &Picker) {
    let area = tui::centered(area, 60, 80);
    frame.render_widget(Clear, area);
    let mut items: Vec<ListItem> = Vec::new();
    let mut section = "";
    let mut cursor_item = 0;
    for (i, line) in app.facets.iter().enumerate() {
        let (heading, name) = match &line.facet {
            Facet::Harness(side) => ("harness", side.key().to_string()),
            Facet::Project(name) => ("project", name.clone()),
        };
        if heading != section {
            section = heading;
            items.push(ListItem::new(Line::from(Span::styled(
                heading,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))));
        }
        if i == app.facet_cursor {
            cursor_item = items.len();
        }
        let shown = app.facet_shown(&line.facet);
        let style = if shown { Style::default() } else { Style::default().fg(DIM) };
        items.push(ListItem::new(Line::from(Span::styled(
            format!(
                "  {} {:<32} {:>5} · {:>9}",
                if shown { "[x]" } else { "[ ]" },
                clip(&name, 32),
                line.sessions,
                human_size(line.bytes)
            ),
            style,
        ))));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(Block::bordered().inner(area));
    frame.render_widget(
        Block::bordered().border_type(BorderType::Rounded).title(" filters ").title_alignment(Alignment::Center),
        area,
    );
    let height = usize::from(chunks[0].height);
    let mut state = ListState::default().with_offset(centered_offset(cursor_item, items.len(), height));
    state.select(Some(cursor_item));
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, chunks[0], &mut state);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " space show/hide · o only this · a show all · esc close",
            Style::default().fg(DIM),
        ))),
        chunks[1],
    );
}

fn draw_confirm(frame: &mut Frame, area: Rect, app: &Picker) {
    let area = tui::centered(area, 60, 55);
    frame.render_widget(Clear, area);
    let candidates = app.confirm_candidates();
    let total_bytes: u64 = candidates.iter().map(|&i| app.rows[i].facts.bytes).sum();
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{} {} session(s) · {}?", app.verb, candidates.len(), human_size(total_bytes)),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for &i in candidates.iter().take(10) {
        let row = &app.rows[i];
        let first = row.columns.first().map_or("—", String::as_str);
        let last = row.columns.last().map_or("", String::as_str);
        lines.push(Line::from(Span::styled(format!("  {first} … {last}"), Style::default().fg(DIM))));
    }
    if candidates.len() > 10 {
        lines.push(Line::from(Span::styled(
            format!("  … and {} more", candidates.len() - 10),
            Style::default().fg(DIM),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("y / enter confirm · n / esc back", Style::default().fg(DIM))));
    let paragraph = Paragraph::new(Text::from(lines)).alignment(Alignment::Center).wrap(Wrap { trim: true }).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(format!(" {} ", app.verb))
            .title_alignment(Alignment::Center),
    );
    frame.render_widget(paragraph, area);
}

// Selection, sorting, and filtering rules only; no terminal.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::tests::facts;

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 1_000 * DAY;

    fn row(harness: Side, id: &str, path: &str, age_days: i64, bytes: u64, blocked: bool) -> Row {
        Row {
            columns: vec![id.to_string()],
            facts: facts(harness, id, path, NOW - age_days * DAY, bytes),
            details: Vec::new(),
            blocked: blocked.then(|| "blocked".to_string()),
        }
    }

    fn picker(rows: Vec<Row>, threshold: Option<u32>) -> Picker {
        Picker::new(Spec {
            title: "test".into(),
            verb: "pack".into(),
            headers: vec!["id".into()],
            rows,
            threshold_days: threshold,
            sort: Sort { key: SortKey::Used, reverse: false },
            now_ms: NOW,
        })
    }

    fn shown_ids(p: &Picker) -> Vec<&str> {
        p.view.iter().map(|&i| p.rows[i].facts.id.as_str()).collect()
    }

    #[test]
    fn threshold_hides_younger_rows() {
        let mut p = picker(
            vec![row(Side::Pi, "young", "/a", 5, 1, false), row(Side::Pi, "old", "/a", 40, 1, false)],
            Some(30),
        );
        assert_eq!(shown_ids(&p), vec!["old"]);
        p.step_threshold(false);
        p.step_threshold(false);
        assert_eq!((p.threshold, shown_ids(&p)), (7, vec!["old"]));
        p.step_threshold(false);
        assert_eq!((p.threshold, shown_ids(&p)), (0, vec!["old", "young"]), "oldest first");
    }

    #[test]
    fn confirm_is_selected_intersect_shown() {
        let mut p = picker(vec![row(Side::Pi, "a", "/p", 50, 1, false), row(Side::Pi, "b", "/p", 50, 1, false)], None);
        p.selected = vec![true, true];
        p.filter = "b".into();
        p.reindex();
        assert_eq!(p.confirm_candidates(), vec![1]);
    }

    #[test]
    fn confirm_falls_back_to_cursor_when_none_selected() {
        let mut p = picker(vec![row(Side::Pi, "a", "/p", 60, 1, false), row(Side::Pi, "b", "/p", 50, 1, false)], None);
        p.cursor = 1;
        assert_eq!(p.confirm_candidates(), vec![1]);
    }

    #[test]
    fn path_toggle_selects_then_deselects_shown_selectable_rows() {
        let mut p = picker(
            vec![
                row(Side::Pi, "a1", "/g1", 30, 1, false),
                row(Side::Pi, "a2", "/g1", 20, 1, false),
                row(Side::Pi, "a3", "/g1", 10, 1, true),
                row(Side::Pi, "b1", "/g2", 5, 1, false),
            ],
            None,
        );
        p.toggle_path_at_cursor();
        assert_eq!(p.selected, vec![true, true, false, false]);
        p.toggle_path_at_cursor();
        assert_eq!(p.selected, vec![false; 4]);
        p.cursor = 2;
        p.toggle_at_cursor();
        p.toggle_all_shown();
        assert!(!p.selected[2], "blocked rows never enter the selection");
    }

    #[test]
    fn sort_keys_reorder_and_repeat_to_reverse() {
        let mut p = picker(
            vec![
                row(Side::Pi, "small-old", "/z/beta", 90, 10, false),
                row(Side::Dsh, "big-new", "/a/alpha", 1, 99, false),
            ],
            None,
        );
        assert_eq!(shown_ids(&p), vec!["small-old", "big-new"]);
        p.sort_by(SortKey::Size);
        assert_eq!(shown_ids(&p), vec!["big-new", "small-old"]);
        p.sort_by(SortKey::Size);
        assert_eq!(shown_ids(&p), vec!["small-old", "big-new"], "smallest first");
        p.sort_by(SortKey::Project);
        assert_eq!(shown_ids(&p), vec!["big-new", "small-old"], "alpha before beta");
    }

    #[test]
    fn hidden_harnesses_and_projects_leave_the_view_and_the_selection() {
        let mut p = picker(
            vec![
                row(Side::Pi, "pi1", "/w/alpha", 50, 1, false),
                row(Side::Dsh, "dsh1", "/w/alpha", 50, 1, false),
                row(Side::Claude, "cc1", "/w/beta", 50, 1, false),
            ],
            None,
        );
        p.toggle_all_shown();
        p.facet_cursor = p.facets.iter().position(|l| l.facet == Facet::Harness(Side::Dsh)).unwrap();
        p.toggle_facet();
        assert_eq!(p.confirm_candidates(), vec![0, 2]);
        p.facet_cursor = p.facets.iter().position(|l| l.facet == Facet::Project("beta".into())).unwrap();
        p.only_facet();
        assert_eq!(shown_ids(&p), vec!["cc1"]);
        assert_eq!(p.hiding().as_deref(), Some("−dsh −alpha"));
        p.show_all_facets();
        assert_eq!(p.view.len(), 3);
    }

    #[test]
    fn select_all_takes_the_shown_rows_and_a_second_press_releases_them() {
        let mut p = picker(
            vec![
                row(Side::Pi, "a", "/w/alpha", 50, 1, false),
                row(Side::Pi, "b", "/w/beta", 50, 1, false),
                row(Side::Pi, "c", "/w/beta", 50, 1, true),
            ],
            None,
        );
        p.filter = "b".into();
        p.reindex();
        assert_eq!(p.shown_check(), "[ ]");
        p.toggle_all_shown();
        assert_eq!(p.selected, vec![false, true, false], "only shown, selectable rows");
        assert_eq!(p.shown_check(), "[x]");
        p.toggle_all_shown();
        assert_eq!(p.selected, vec![false; 3]);
        p.toggle_at_cursor();
        assert_eq!(p.shown_check(), "[x]", "the blocked row does not count");
    }

    #[test]
    fn the_filter_panel_lists_only_what_the_threshold_and_search_leave() {
        let mut p = picker(
            vec![
                row(Side::Pi, "old", "/w/alpha", 50, 10, false),
                row(Side::Claude, "young", "/w/beta", 3, 20, false),
            ],
            Some(30),
        );
        let listed = |p: &Picker| p.facets.iter().map(|l| (l.facet.clone(), l.sessions)).collect::<Vec<_>>();
        assert_eq!(listed(&p), vec![(Facet::Harness(Side::Pi), 1), (Facet::Project("alpha".into()), 1)]);
        p.step_threshold(false);
        p.step_threshold(false);
        p.step_threshold(false);
        assert_eq!(p.facets.len(), 4, "a lower threshold brings claude and beta back");
        p.filter = "you".into();
        p.reindex();
        assert_eq!(listed(&p), vec![(Facet::Harness(Side::Claude), 1), (Facet::Project("beta".into()), 1)]);
    }

    #[test]
    fn the_cursor_stays_centred_until_an_end_of_the_list() {
        let (len, height) = (100, 11);
        assert_eq!(centered_offset(0, len, height), 0);
        assert_eq!(centered_offset(5, len, height), 0);
        assert_eq!(centered_offset(6, len, height), 1, "five lines above, five below");
        assert_eq!(centered_offset(50, len, height), 45);
        assert_eq!(centered_offset(94, len, height), 89);
        assert_eq!(centered_offset(99, len, height), 89, "pinned to the bottom");
        assert_eq!(centered_offset(3, 8, height), 0, "a list that fits never scrolls");
    }
}
