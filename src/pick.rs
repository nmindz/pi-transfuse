//! Multi-select session picker for the archive subcommands (pack, unpack, push, pull).
//!
//! Callers pass [`Spec`] rows; this module handles sorting, filtering, the age cutoff, and confirm.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
    /// Command settings the user can change before confirming.
    pub settings: Vec<Setting>,
    /// Extra (key, what) rows appended under a blank line in the help overlay.
    pub extra_keys: Vec<(String, String)>,
    /// Home of the host the rows' paths belong to, shown as `~`; this host's
    /// when `None`.
    pub home: Option<String>,
}

/// Value of one command setting the picker lets the user change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Flag(bool),
    Text(String),
}

/// A command setting shown in the top bar and the confirm dialog.
#[derive(Debug, Clone)]
pub struct Setting {
    /// Key that toggles a flag or opens the text editor; never one the picker already uses.
    pub key: char,
    pub label: String,
    pub value: Value,
    /// Shown in the warning color while a flag is on or a text is set (destructive or notable).
    pub warn: bool,
}

impl Setting {
    /// A flag that is on, or a text that is set.
    fn active(&self) -> bool {
        match &self.value {
            Value::Flag(on) => *on,
            Value::Text(text) => !text.is_empty(),
        }
    }

    fn shown(&self) -> String {
        match &self.value {
            Value::Flag(on) => if *on { "on" } else { "off" }.to_string(),
            Value::Text(text) if text.is_empty() => "—".to_string(),
            Value::Text(text) => text.clone(),
        }
    }

    fn style(&self, base: Style) -> Style {
        if self.warn && self.active() {
            base.fg(WARN)
        } else {
            base
        }
    }
}

/// What the user confirmed: row indices (ascending) and the settings as left.
pub struct Picked {
    pub rows: Vec<usize>,
    pub settings: Vec<Setting>,
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
    /// A directory holding several session paths; hides or keeps all below it.
    Path(String),
    Untitled,
}

struct FacetLine {
    facet: Facet,
    sessions: usize,
    bytes: u64,
}

/// Opaque saved view for rebuilding the picker over fresh rows.
pub struct Keep {
    selected: HashSet<(String, bool)>,
    cursor_key: Option<(String, bool)>,
    sort: Sort,
    filter: String,
    threshold: u32,
    threshold_enabled: bool,
    hidden_harnesses: HashSet<Side>,
    hidden_projects: HashSet<String>,
    hidden_untitled: bool,
    hidden_paths: HashSet<String>,
    only_paths: HashSet<String>,
    settings: Vec<Setting>,
}

fn row_key(row: &Row) -> (String, bool) {
    (row.facts.id.clone(), row.facts.archive_bytes.is_some())
}

fn title_empty(facts: &Facts) -> bool {
    facts.title.trim().is_empty()
}

// In-memory picker; draw only reads this.
pub struct Picker {
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
    hidden_untitled: bool,
    /// Directories whose sessions are hidden, at any depth below.
    hidden_paths: HashSet<String>,
    /// When any, only sessions at or below one of these directories show.
    only_paths: HashSet<String>,
    /// Harnesses, projects, paths and untitled present among the rows.
    facets: Vec<FacetLine>,
    /// What is typed in the filter panel; narrows its lines by fuzzy match.
    facet_query: String,
    /// Indices into `facets` the query leaves, best match first per section.
    facet_view: Vec<usize>,
    /// Position in `facet_view`.
    facet_cursor: usize,
    selected: Vec<bool>,
    /// Indices into `rows` in display order.
    view: Vec<usize>,
    cursor: usize,
    filter: String,
    editing_filter: bool,
    settings: Vec<Setting>,
    extra_keys: Vec<(String, String)>,
    home: String,
    /// Setting whose text is being edited, with the value to restore on Esc.
    editing_setting: Option<(usize, String)>,
    overlay: Overlay,
    status: String,
    status_color: Color,
    embedded: bool,
    cancelled: bool,
    confirmed: Option<Vec<usize>>,
}

impl Picker {
    pub fn new(spec: Spec) -> Self {
        Self::from_spec(spec, false)
    }

    pub fn embedded(spec: Spec) -> Self {
        Self::from_spec(spec, true)
    }

    fn from_spec(spec: Spec, embedded: bool) -> Self {
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
            hidden_untitled: false,
            hidden_paths: HashSet::new(),
            only_paths: HashSet::new(),
            facets: Vec::new(),
            facet_query: String::new(),
            facet_view: Vec::new(),
            facet_cursor: 0,
            selected: vec![false; n],
            view: Vec::new(),
            cursor: 0,
            filter: String::new(),
            editing_filter: false,
            settings: spec.settings,
            extra_keys: spec.extra_keys,
            home: spec.home.unwrap_or_else(|| crate::paths::home().display().to_string()),
            editing_setting: None,
            overlay: Overlay::None,
            status: String::new(),
            status_color: DIM,
            embedded,
            cancelled: false,
            confirmed: None,
        };
        picker.reindex();
        picker
    }

    pub fn handle(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.editing_setting.is_some() {
            setting_key(self, key.code);
        } else if self.editing_filter {
            filter_key(self, key.code);
        } else {
            match self.overlay {
                Overlay::Help => self.overlay = Overlay::None,
                Overlay::Confirm => match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => {
                        self.confirmed = Some(self.confirm_candidates());
                    }
                    KeyCode::Char('n') | KeyCode::Esc => self.overlay = Overlay::None,
                    _ => {}
                },
                Overlay::Filters => facet_key(self, key),
                Overlay::None => list_key(self, key.code),
            }
        }
    }

    pub fn captures_keys(&self) -> bool {
        self.editing_setting.is_some()
            || self.editing_filter
            || !matches!(self.overlay, Overlay::None)
    }

    pub fn request_confirm(&mut self) {
        self.request_confirm_inner();
    }

    pub fn take_confirmed(&mut self) -> Option<Picked> {
        let rows = self.confirmed.take()?;
        self.overlay = Overlay::None;
        Some(Picked { rows, settings: self.settings.clone() })
    }

    pub fn set_title(&mut self, title: String) {
        self.title = title;
    }

    pub fn set_status(&mut self, msg: impl Into<String>, color: Color) {
        self.set_status_inner(msg, color);
    }

    pub fn settings(&self) -> &[Setting] {
        &self.settings
    }

    pub fn keep(&self) -> Keep {
        let selected = self
            .selected
            .iter()
            .enumerate()
            .filter(|(_, &on)| on)
            .map(|(i, _)| row_key(&self.rows[i]))
            .collect();
        let cursor_key = self.cursor_row_index().map(|i| row_key(&self.rows[i]));
        Keep {
            selected,
            cursor_key,
            sort: self.sort,
            filter: self.filter.clone(),
            threshold: self.threshold,
            threshold_enabled: self.threshold_enabled,
            hidden_harnesses: self.hidden_harnesses.clone(),
            hidden_projects: self.hidden_projects.clone(),
            hidden_untitled: self.hidden_untitled,
            hidden_paths: self.hidden_paths.clone(),
            only_paths: self.only_paths.clone(),
            settings: self.settings.clone(),
        }
    }

    pub fn restore(&mut self, keep: Keep) {
        self.sort = keep.sort;
        self.filter = keep.filter;
        self.threshold = keep.threshold;
        self.threshold_enabled = keep.threshold_enabled;
        self.hidden_harnesses = keep.hidden_harnesses;
        self.hidden_projects = keep.hidden_projects;
        self.hidden_untitled = keep.hidden_untitled;
        self.hidden_paths = keep.hidden_paths;
        self.only_paths = keep.only_paths;
        self.settings = keep.settings;
        // A row that became blocked, say moved away meanwhile, stays unselected.
        self.selected = self
            .rows
            .iter()
            .map(|row| row.blocked.is_none() && keep.selected.contains(&row_key(row)))
            .collect();
        self.reindex();
        self.cursor = keep
            .cursor_key
            .and_then(|key| self.view.iter().position(|&i| row_key(&self.rows[i]) == key))
            .unwrap_or_else(|| self.cursor.min(self.view.len().saturating_sub(1)));
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        draw_picker(frame, area, self);
    }

    /// Filter, threshold, sort, then keep the cursor on the same row when possible.
    /// The filter panel lists only what the text filter and threshold leave.
    fn reindex(&mut self) {
        let needle = self.filter.to_lowercase();
        let eligible: Vec<usize> = (0..self.rows.len()).filter(|&i| self.eligible(i, &needle)).collect();
        let old_facet = self.facet_at_cursor().map(|l| l.facet.clone());
        self.facets = facets(eligible.iter().map(|&i| &self.rows[i]));
        self.refacet(old_facet);

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
        self.hidden_harnesses.contains(&facts.harness)
            || self.hidden_projects.contains(&facts.project)
            || (self.hidden_untitled && title_empty(facts))
            || self.hidden_paths.iter().any(|dir| under(&facts.path, dir))
            || (!self.only_paths.is_empty() && !self.only_paths.iter().any(|dir| under(&facts.path, dir)))
    }

    /// Narrows the panel's lines to the query and keeps the cursor on `keep`
    /// when it is still listed.
    fn refacet(&mut self, keep: Option<Facet>) {
        let mut view: Vec<(usize, usize, usize)> = self
            .facets
            .iter()
            .enumerate()
            .filter_map(|(i, line)| {
                let texts = self.facet_texts(&line.facet);
                let score = texts.iter().filter_map(|t| fuzzy(t, &self.facet_query)).min()?;
                Some((facet_rank(&line.facet), score, i))
            })
            .collect();
        view.sort_unstable();
        self.facet_view = view.into_iter().map(|(_, _, i)| i).collect();
        self.facet_cursor = keep
            .and_then(|f| self.facet_view.iter().position(|&i| self.facets[i].facet == f))
            .unwrap_or_else(|| self.facet_cursor.min(self.facet_view.len().saturating_sub(1)));
    }

    fn facet_at_cursor(&self) -> Option<&FacetLine> {
        self.facet_view.get(self.facet_cursor).and_then(|&i| self.facets.get(i))
    }

    /// Changes the panel's query; the cursor goes to the best match.
    fn edit_facet_query(&mut self, edit: impl FnOnce(&mut String)) {
        edit(&mut self.facet_query);
        self.facet_cursor = 0;
        self.refacet(None);
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
            Facet::Path(dir) => {
                !self.hidden_paths.iter().any(|h| under(dir, h))
                    && (self.only_paths.is_empty() || self.only_paths.iter().any(|o| under(dir, o)))
            }
            Facet::Untitled => !self.hidden_untitled,
        }
    }

    fn toggle_facet(&mut self) {
        let Some(facet) = self.facet_at_cursor().map(|l| l.facet.clone()) else {
            return;
        };
        match facet {
            Facet::Harness(side) => flip(&mut self.hidden_harnesses, side),
            Facet::Project(name) => flip(&mut self.hidden_projects, name),
            Facet::Path(dir) => {
                if self.facet_shown(&Facet::Path(dir.clone())) {
                    if !self.only_paths.remove(&dir) {
                        self.hidden_paths.insert(dir);
                    }
                } else {
                    self.hidden_paths.retain(|h| !under(&dir, h));
                    if !self.only_paths.is_empty() && !self.only_paths.iter().any(|o| under(&dir, o)) {
                        self.only_paths.insert(dir);
                    }
                }
            }
            Facet::Untitled => self.hidden_untitled = !self.hidden_untitled,
        }
        self.reindex();
    }

    /// Shows only the cursor's facet among its kind: one harness, one
    /// project, or the sessions under one directory.
    fn only_facet(&mut self) {
        let Some(facet) = self.facet_at_cursor().map(|l| l.facet.clone()) else {
            return;
        };
        self.only_matching(|f| *f == facet);
    }

    /// Shows only the lines the query matches, among the cursor's kind.
    fn only_matches(&mut self) {
        let Some(rank) = self.facet_at_cursor().map(|l| facet_rank(&l.facet)) else {
            return;
        };
        let matched: Vec<Facet> = self
            .facet_view
            .iter()
            .map(|&i| self.facets[i].facet.clone())
            .filter(|f| facet_rank(f) == rank)
            .collect();
        self.only_matching(|f| matched.contains(f));
    }

    /// Hides every line of `keep`'s kind that `keep` rejects; for paths, keeps
    /// only the sessions under the accepted directories.
    fn only_matching(&mut self, keep: impl Fn(&Facet) -> bool) {
        let Some(rank) = self.facets.iter().find(|l| keep(&l.facet)).map(|l| facet_rank(&l.facet)) else {
            return;
        };
        let kind = self.facets.iter().map(|l| &l.facet).filter(|f| facet_rank(f) == rank);
        match rank {
            0 => {
                self.hidden_harnesses = kind
                    .filter(|f| !keep(f))
                    .filter_map(|f| if let Facet::Harness(side) = f { Some(*side) } else { None })
                    .collect();
            }
            1 => {
                self.hidden_projects = kind
                    .filter(|f| !keep(f))
                    .filter_map(|f| if let Facet::Project(name) = f { Some(name.clone()) } else { None })
                    .collect();
            }
            2 => {
                self.only_paths = kind
                    .filter(|f| keep(f))
                    .filter_map(|f| if let Facet::Path(dir) = f { Some(dir.clone()) } else { None })
                    .collect();
                let only = &self.only_paths;
                self.hidden_paths.retain(|h| !only.iter().any(|o| under(o, h)));
            }
            _ => self.hidden_untitled = !self.hidden_untitled,
        }
        self.reindex();
    }

    fn show_all_facets(&mut self) {
        self.hidden_harnesses.clear();
        self.hidden_projects.clear();
        self.hidden_paths.clear();
        self.only_paths.clear();
        self.hidden_untitled = false;
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
        if self.hidden_untitled {
            parts.push("−untitled".into());
        }
        let tilde = |dir: &String| self.tilde(dir);
        match self.only_paths.iter().collect::<Vec<_>>().as_slice() {
            [] => {}
            [one] => parts.push(format!("only {}", tilde(one))),
            many => parts.push(format!("only {} paths", many.len())),
        }
        match self.hidden_paths.iter().collect::<Vec<_>>().as_slice() {
            [] => {}
            [one] => parts.push(format!("−{}", tilde(one))),
            many => parts.push(format!("−{} paths", many.len())),
        }
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    fn set_status_inner<S: Into<String>>(&mut self, msg: S, color: Color) {
        self.status = msg.into();
        self.status_color = color;
    }

    fn request_confirm_inner(&mut self) {
        if self.confirm_candidates().is_empty() {
            self.set_status_inner("nothing selected to act on", WARN);
            return;
        }
        self.overlay = Overlay::Confirm;
    }

    /// Flips the flag or opens the editor of the setting on `key`; false when none is.
    fn press_setting(&mut self, key: char) -> bool {
        let Some(i) = self.settings.iter().position(|s| s.key == key) else {
            return false;
        };
        let setting = &mut self.settings[i];
        match &mut setting.value {
            Value::Flag(on) => *on = !*on,
            Value::Text(text) => {
                self.editing_setting = Some((i, text.clone()));
                return true;
            }
        }
        let msg = format!("{}: {}", setting.label, setting.shown());
        let color = if setting.warn && setting.active() { WARN } else { DIM };
        self.set_status_inner(msg, color);
        true
    }

    fn toggle_hidden_untitled(&mut self) {
        self.hidden_untitled = !self.hidden_untitled;
        self.reindex();
        let msg = if self.hidden_untitled {
            "untitled sessions hidden"
        } else {
            "untitled sessions shown"
        };
        self.set_status_inner(msg, DIM);
    }
}

fn flip<T: Eq + std::hash::Hash>(set: &mut HashSet<T>, item: T) {
    if !set.remove(&item) {
        set.insert(item);
    }
}

/// Harnesses in listing order, projects largest first, the directories that
/// group several session paths, then untitled, with their totals.
fn facets<'a>(rows: impl Iterator<Item = &'a Row>) -> Vec<FacetLine> {
    let rows: Vec<&Row> = rows.collect();
    let mut harnesses: BTreeMap<&str, (Side, usize, u64)> = BTreeMap::new();
    let mut projects: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    let mut untitled = (0usize, 0u64);
    for row in rows.iter().copied() {
        let f = &row.facts;
        let h = harnesses.entry(f.harness.key()).or_insert((f.harness, 0, 0));
        h.1 += 1;
        h.2 += f.bytes;
        let p = projects.entry(f.project.as_str()).or_default();
        p.0 += 1;
        p.1 += f.bytes;
        if title_empty(f) {
            untitled.0 += 1;
            untitled.1 += f.bytes;
        }
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
    out.extend(path_lines(&rows));
    if untitled.0 > 0 {
        out.push(FacetLine {
            facet: Facet::Untitled,
            sessions: untitled.0,
            bytes: untitled.1,
        });
    }
    out
}

/// The confirmed rows and settings, or `None` if the user cancelled.
pub fn pick(spec: Spec) -> Result<Option<Picked>> {
    let mut terminal = ratatui::init();
    let result = run_picker(&mut terminal, spec);
    ratatui::restore();
    result
}

// Key handling; overlays eat keys before list mode sees them.
fn run_picker(terminal: &mut DefaultTerminal, spec: Spec) -> Result<Option<Picked>> {
    let mut app = Picker::new(spec);
    while !app.cancelled && app.confirmed.is_none() {
        terminal.draw(|frame| app.draw(frame, frame.area()))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            app.cancelled = true;
        } else {
            app.handle(key);
        }
    }
    if app.cancelled {
        return Ok(None);
    }
    Ok(app.take_confirmed())
}

/// Text editor for one setting: Enter keeps the text, Esc restores the old one.
fn setting_key(app: &mut Picker, code: KeyCode) {
    let Some((i, previous)) = app.editing_setting.take() else {
        return;
    };
    let Some(Value::Text(text)) = app.settings.get_mut(i).map(|s| &mut s.value) else {
        return;
    };
    match code {
        KeyCode::Esc => *text = previous,
        KeyCode::Enter => {}
        KeyCode::Backspace => {
            text.pop();
            app.editing_setting = Some((i, previous));
        }
        KeyCode::Char(c) => {
            text.push(c);
            app.editing_setting = Some((i, previous));
        }
        _ => app.editing_setting = Some((i, previous)),
    }
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

/// The filter panel: typing narrows its lines, the arrows move, `space`
/// toggles, Ctrl-O keeps only the line, Ctrl-S only the matching lines of
/// its kind, Ctrl-A shows everything.
fn facet_key(app: &mut Picker, key: KeyEvent) {
    let last = app.facet_view.len().saturating_sub(1);
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('o') => app.only_facet(),
            KeyCode::Char('s') => app.only_matches(),
            KeyCode::Char('a') => app.show_all_facets(),
            KeyCode::Char('u') => app.edit_facet_query(String::clear),
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Esc if !app.facet_query.is_empty() => app.edit_facet_query(String::clear),
        KeyCode::Esc | KeyCode::Enter => app.overlay = Overlay::None,
        KeyCode::Down => app.facet_cursor = (app.facet_cursor + 1).min(last),
        KeyCode::Up => app.facet_cursor = app.facet_cursor.saturating_sub(1),
        KeyCode::PageDown => app.facet_cursor = (app.facet_cursor + 10).min(last),
        KeyCode::PageUp => app.facet_cursor = app.facet_cursor.saturating_sub(10),
        KeyCode::Home => app.facet_cursor = 0,
        KeyCode::End => app.facet_cursor = last,
        KeyCode::Char(' ') => app.toggle_facet(),
        KeyCode::Backspace => app.edit_facet_query(|q| {
            q.pop();
        }),
        KeyCode::Char(c) => app.edit_facet_query(|q| q.push(c)),
        _ => {}
    }
}

/// True when `path` is `dir` or lies below it, on whole components.
fn under(path: &str, dir: &str) -> bool {
    std::path::Path::new(path).starts_with(dir)
}

/// Section order of the panel.
fn facet_rank(facet: &Facet) -> usize {
    match facet {
        Facet::Harness(_) => 0,
        Facet::Project(_) => 1,
        Facet::Path(_) => 2,
        Facet::Untitled => 3,
    }
}

impl Picker {
    /// A directory with this picker's home shown as `~`.
    fn tilde(&self, dir: &str) -> String {
        match std::path::Path::new(dir).strip_prefix(&self.home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => dir.to_string(),
        }
    }

    /// What the panel's query is matched against.
    fn facet_texts(&self, facet: &Facet) -> Vec<String> {
        match facet {
            Facet::Harness(side) => vec![side.key().to_string()],
            Facet::Project(name) => vec![name.clone()],
            Facet::Path(dir) => vec![self.tilde(dir), dir.clone()],
            Facet::Untitled => vec!["(untitled)".to_string()],
        }
    }
}

/// How well `needle` matches `hay`, lower is better, ignoring case: a
/// substring ranks by where it starts; otherwise the needle's characters in
/// order rank behind every substring, by how far they spread.
fn fuzzy(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let (hay, needle) = (hay.to_lowercase(), needle.to_lowercase());
    if let Some(at) = hay.find(&needle) {
        return Some(at);
    }
    let mut chars = hay.char_indices();
    let (mut first, mut last) = (None, 0);
    for wanted in needle.chars() {
        let (at, _) = chars.find(|&(_, c)| c == wanted)?;
        first.get_or_insert(at);
        last = at;
    }
    Some(1 << 20 | (last - first.unwrap_or(last)))
}

/// Directories above the session paths that hold more than one of them,
/// leaving out any that holds every one, since hiding or keeping it would
/// change nothing: `~/Projects/_myself` when several projects sit there.
fn path_lines(rows: &[&Row]) -> Vec<FacetLine> {
    let paths: BTreeMap<&str, (usize, u64)> = rows.iter().fold(BTreeMap::new(), |mut acc, row| {
        let entry = acc.entry(row.facts.path.as_str()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += row.facts.bytes;
        acc
    });
    let mut dirs: BTreeMap<String, (usize, usize, u64)> = BTreeMap::new();
    for (path, (sessions, bytes)) in &paths {
        for dir in std::path::Path::new(path).ancestors().skip(1) {
            let Some(dir) = dir.to_str().filter(|d| !d.is_empty() && *d != "/") else { continue };
            let entry = dirs.entry(dir.to_string()).or_insert((0, 0, 0));
            entry.0 += 1;
            entry.1 += sessions;
            entry.2 += bytes;
        }
    }
    dirs.into_iter()
        .filter(|(_, (held, _, _))| *held > 1 && *held < paths.len())
        .map(|(dir, (_, sessions, bytes))| FacetLine { facet: Facet::Path(dir), sessions, bytes })
        .collect()
}

fn list_key(app: &mut Picker, code: KeyCode) {
    let last = app.view.len().saturating_sub(1);
    match code {
        KeyCode::Char('q') | KeyCode::Esc if !app.embedded => app.cancelled = true,
        KeyCode::Char('q') | KeyCode::Esc => {}
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
        KeyCode::Enter => app.request_confirm_inner(),
        KeyCode::Char('?') => app.overlay = Overlay::Help,
        KeyCode::Char('U') => {
            if !app.press_setting('U') {
                app.toggle_hidden_untitled();
            }
        }
        KeyCode::Char(c) => {
            if app.press_setting(c) {
                return;
            }
            if let Some(key) = SortKey::ALL.into_iter().find(|k| k.key() == c) {
                app.sort_by(key);
            }
        }
        _ => {}
    }
}

// Ratatui panes; colors match `crate::tui`.
fn draw_picker(frame: &mut Frame, area: Rect, app: &Picker) {
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
    let base = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let mut parts = vec![(app.title.clone(), base)];
    for (i, setting) in app.settings.iter().enumerate() {
        let editing = matches!(app.editing_setting, Some((e, _)) if e == i);
        let text = match &setting.value {
            Value::Text(text) if editing => format!("{}: {text}▏", setting.label),
            _ => format!("{}: {}", setting.label, setting.shown()),
        };
        parts.push((text, setting.style(base)));
    }
    if app.threshold_enabled {
        parts.push((format!("unused ≥ {}d", app.threshold), base));
    }
    parts.push((app.sort.describe(), base));
    if let Some(hiding) = app.hiding() {
        parts.push((hiding, base));
    }
    if app.editing_filter {
        parts.push((format!("/{}", app.filter), base));
    } else if !app.filter.is_empty() {
        parts.push((format!("filter: {}", app.filter), base));
    }
    let mut spans = vec![Span::styled(" ", base)];
    for (n, (text, style)) in parts.into_iter().enumerate() {
        if n > 0 {
            spans.push(Span::styled(" · ", base));
        }
        spans.push(Span::styled(text, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
        let mut hint = String::from("j/k move · space toggle · a all · / search · f filters · ");
        hint.push_str("U hide untitled · u s n d sort · enter confirm · ");
        if app.embedded {
            hint.push_str("? keys");
        } else {
            hint.push_str("q quit");
        }
        for setting in &app.settings {
            hint.push_str(&format!(" · {} {}", setting.key, setting.label));
        }
        (hint, DIM)
    } else {
        (app.status.clone(), app.status_color)
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
        ("f", "filter panel: type to narrow it; harnesses, projects, paths, untitled"),
        ("U", "hide or show sessions without a title"),
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
    for setting in &app.settings {
        let verb = match setting.value {
            Value::Flag(_) => "toggle",
            Value::Text(_) => "edit",
        };
        rows.push((setting.key.to_string(), format!("{verb} {}", setting.label)));
    }
    rows.extend([("enter".into(), "confirm selection".into())]);
    if app.embedded {
        rows.push(("?".into(), "this help".into()));
    } else {
        rows.push(("? / q".into(), "this help / cancel".into()));
    }
    if !app.extra_keys.is_empty() {
        rows.push((String::new(), String::new()));
        rows.extend(app.extra_keys.iter().cloned());
    }
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
    let area = tui::centered(area, 70, 80);
    frame.render_widget(Clear, area);
    let mut items: Vec<ListItem> = Vec::new();
    let mut section = "";
    let mut cursor_item = 0;
    let width = usize::from(area.width.saturating_sub(26)).clamp(16, 64);
    for (pos, line) in app.facet_view.iter().map(|&i| &app.facets[i]).enumerate() {
        let (heading, name) = match &line.facet {
            Facet::Harness(side) => ("harness", side.key().to_string()),
            Facet::Project(name) => ("project", name.clone()),
            Facet::Path(dir) => ("path", app.tilde(dir)),
            Facet::Untitled => ("title", "(untitled)".to_string()),
        };
        if heading != section {
            section = heading;
            items.push(ListItem::new(Line::from(Span::styled(
                heading,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))));
        }
        if pos == app.facet_cursor {
            cursor_item = items.len();
        }
        let shown = app.facet_shown(&line.facet);
        let style = if shown { Style::default() } else { Style::default().fg(DIM) };
        // Paths keep their tail, where the distinguishing part sits.
        let name = if matches!(line.facet, Facet::Path(_)) { crate::shorten(&name, width) } else { clip(&name, width) };
        items.push(ListItem::new(Line::from(Span::styled(
            format!(
                "  {} {:<width$} {:>5} · {:>9}",
                if shown { "[x]" } else { "[ ]" },
                name,
                line.sessions,
                human_size(line.bytes)
            ),
            style,
        ))));
    }
    if items.is_empty() {
        items.push(ListItem::new(Span::styled("  nothing matches", Style::default().fg(DIM))));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
        .split(Block::bordered().inner(area));
    frame.render_widget(
        Block::bordered().border_type(BorderType::Rounded).title(" filters ").title_alignment(Alignment::Center),
        area,
    );
    let query = if app.facet_query.is_empty() {
        Span::styled(" type to narrow▏", Style::default().fg(DIM))
    } else {
        Span::styled(format!(" {}▏", app.facet_query), Style::default().fg(ACCENT))
    };
    frame.render_widget(Paragraph::new(Line::from(query)), chunks[0]);
    let height = usize::from(chunks[1].height);
    let mut state = ListState::default().with_offset(centered_offset(cursor_item, items.len(), height));
    state.select((!app.facet_view.is_empty()).then_some(cursor_item));
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, chunks[1], &mut state);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑↓ move · space show/hide · ^O only this · ^S only matches · ^A show all · esc clear/close",
            Style::default().fg(DIM),
        ))),
        chunks[2],
    );
}

fn draw_confirm(frame: &mut Frame, area: Rect, app: &Picker) {
    let active: Vec<&Setting> = app.settings.iter().filter(|s| s.active()).collect();
    let height = 55 + 5 * active.len().min(6) as u16;
    let area = tui::centered(area, 60, height);
    frame.render_widget(Clear, area);
    let candidates = app.confirm_candidates();
    let total_bytes: u64 = candidates.iter().map(|&i| app.rows[i].facts.bytes).sum();
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{} {} session(s) · {}?", app.verb, candidates.len(), human_size(total_bytes)),
            Style::default().add_modifier(Modifier::BOLD),
        )),
    ];
    for setting in active {
        let text = format!("{}: {}", setting.label, setting.shown());
        lines.push(Line::from(Span::styled(text, setting.style(Style::default()))));
    }
    lines.push(Line::from(""));
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
        picker_with(rows, threshold, Vec::new())
    }

    fn picker_with(rows: Vec<Row>, threshold: Option<u32>, settings: Vec<Setting>) -> Picker {
        Picker::new(Spec {
            title: "test".into(),
            verb: "pack".into(),
            headers: vec!["id".into()],
            rows,
            threshold_days: threshold,
            sort: Sort { key: SortKey::Used, reverse: false },
            now_ms: NOW,
            settings,
            extra_keys: Vec::new(),
            home: None,
        })
    }

    fn row_named(id: &str, title: &str) -> Row {
        let mut f = facts(Side::Pi, id, "/w/p", 50, 10);
        f.title = title.into();
        Row {
            columns: vec![id.into()],
            facts: f,
            details: Vec::new(),
            blocked: None,
        }
    }

    fn setting(key: char, label: &str, value: Value) -> Setting {
        Setting { key, label: label.into(), value, warn: true }
    }

    #[test]
    fn a_flag_key_flips_the_setting_and_reports_it() {
        let delete = setting('D', "delete source", Value::Flag(false));
        let mut p = picker_with(Vec::new(), None, vec![delete]);
        list_key(&mut p, KeyCode::Char('D'));
        assert_eq!(p.settings[0].value, Value::Flag(true));
        assert_eq!((p.status.as_str(), p.status_color), ("delete source: on", WARN));
        list_key(&mut p, KeyCode::Char('D'));
        assert_eq!(p.settings[0].value, Value::Flag(false));
        assert_eq!(p.status, "delete source: off");
    }

    #[test]
    fn a_text_edit_is_kept_on_enter_and_restored_on_esc() {
        let text = |p: &Picker| p.settings[0].value.clone();
        let relocate = setting('R', "relocate to", Value::Text("/a".into()));
        let mut p = picker_with(Vec::new(), None, vec![relocate]);
        list_key(&mut p, KeyCode::Char('R'));
        assert!(p.editing_setting.is_some());
        setting_key(&mut p, KeyCode::Backspace);
        setting_key(&mut p, KeyCode::Char('q'));
        assert!(!p.cancelled, "keys go to the editor while editing");
        setting_key(&mut p, KeyCode::Enter);
        assert_eq!((text(&p), p.editing_setting.is_none()), (Value::Text("/q".into()), true));
        list_key(&mut p, KeyCode::Char('R'));
        setting_key(&mut p, KeyCode::Char('x'));
        assert_eq!(text(&p), Value::Text("/qx".into()));
        setting_key(&mut p, KeyCode::Esc);
        assert_eq!((text(&p), p.editing_setting.is_none()), (Value::Text("/q".into()), true));
    }

    #[test]
    fn a_setting_key_wins_over_a_sort_key() {
        let mut p = picker_with(
            vec![row(Side::Pi, "small", "/a", 90, 1, false), row(Side::Pi, "big", "/b", 1, 9, false)],
            None,
            vec![setting('s', "spare", Value::Flag(false))],
        );
        list_key(&mut p, KeyCode::Char('s'));
        assert_eq!(p.settings[0].value, Value::Flag(true));
        assert_eq!((p.sort.key, shown_ids(&p)), (SortKey::Used, vec!["small", "big"]));
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
    fn u_hides_untitled_rows_and_the_facet_line_counts_them() {
        let rows = vec![
            row_named("titled", "has title"),
            row_named("blank", ""),
            row_named("spaces", "  "),
        ];
        let mut p = picker(rows, None);
        let untitled_line = p
            .facets
            .iter()
            .find(|l| l.facet == Facet::Untitled)
            .expect("untitled facet when some rows lack titles");
        assert_eq!((untitled_line.sessions, untitled_line.bytes), (2, 20));
        list_key(&mut p, KeyCode::Char('U'));
        assert!(p.hidden_untitled);
        assert_eq!(shown_ids(&p), vec!["titled"]);
        list_key(&mut p, KeyCode::Char('U'));
        let mut back = shown_ids(&p).into_iter().map(str::to_string).collect::<Vec<_>>();
        back.sort();
        assert_eq!(back, vec!["blank", "spaces", "titled"]);
    }

    #[test]
    fn embedded_ignores_q_and_esc_in_list_mode() {
        let mut p = Picker::embedded(Spec {
            title: "host".into(),
            verb: "pull".into(),
            headers: vec!["id".into()],
            rows: vec![row(Side::Pi, "a", "/w", 50, 1, false)],
            threshold_days: None,
            sort: Sort { key: SortKey::Used, reverse: false },
            now_ms: NOW,
            settings: Vec::new(),
            extra_keys: Vec::new(),
            home: None,
        });
        list_key(&mut p, KeyCode::Char('q'));
        assert!(!p.cancelled);
        list_key(&mut p, KeyCode::Esc);
        assert!(!p.cancelled);
    }

    #[test]
    fn keep_and_restore_carry_view_state_across_rebuilt_rows() {
        let first = vec![
            row(Side::Pi, "stay", "/w/alpha", 50, 1, false),
            row(Side::Pi, "gone", "/w/beta", 50, 1, false),
        ];
        let mut p = picker(first, None);
        p.selected = vec![true, false];
        p.facet_cursor = p.facets.iter().position(|l| l.facet == Facet::Project("beta".into())).unwrap();
        p.toggle_facet();
        let kept = p.keep();
        let second = vec![
            row(Side::Pi, "stay", "/w/alpha", 50, 1, false),
            row(Side::Pi, "new", "/w/gamma", 50, 1, false),
        ];
        let mut p2 = picker(second, None);
        p2.restore(kept);
        assert_eq!(p2.selected, vec![true, false]);
        assert!(p2.hidden_projects.contains("beta"));
        assert!(shown_ids(&p2).contains(&"stay"));
        assert!(shown_ids(&p2).contains(&"new"));
        p2.cursor = 0;
        p2.toggle_at_cursor();
        assert_eq!(p2.selected, vec![false, false]);
    }

    #[test]
    fn take_confirmed_returns_once_and_closes_the_overlay() {
        let mut p = picker(vec![row(Side::Pi, "a", "/w", 50, 1, false)], None);
        p.selected[0] = true;
        p.request_confirm();
        // A host polls after every key; the open dialog must survive that.
        assert!(p.take_confirmed().is_none());
        assert!(matches!(p.overlay, Overlay::Confirm));
        p.handle(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty()));
        assert_eq!(p.take_confirmed().map(|x| x.rows), Some(vec![0]));
        assert!(p.take_confirmed().is_none());
        assert!(matches!(p.overlay, Overlay::None));
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

    fn press(p: &mut Picker, code: KeyCode, ctrl: bool) {
        let modifiers = if ctrl { KeyModifiers::CONTROL } else { KeyModifiers::empty() };
        p.handle(KeyEvent::new(code, modifiers));
    }

    fn type_text(p: &mut Picker, text: &str) {
        for c in text.chars() {
            press(p, KeyCode::Char(c), false);
        }
    }

    fn panel(p: &Picker) -> Vec<String> {
        p.facet_view
            .iter()
            .map(|&i| match &p.facets[i].facet {
                Facet::Harness(side) => format!("harness {}", side.key()),
                Facet::Project(name) => format!("project {name}"),
                Facet::Path(dir) => format!("path {dir}"),
                Facet::Untitled => "untitled".into(),
            })
            .collect()
    }

    fn projects_tree() -> Picker {
        picker(
            vec![
                row(Side::Pi, "m1", "/h/Projects/_myself/notes", 1, 10, false),
                row(Side::Pi, "m2", "/h/Projects/_myself/blog", 1, 10, false),
                row(Side::Pi, "w1", "/h/Projects/work/api", 1, 10, false),
                row(Side::Pi, "w2", "/h/Projects/work/web", 1, 10, false),
                row(Side::Pi, "t1", "/tmp/scratch", 1, 10, false),
            ],
            None,
        )
    }

    #[test]
    fn only_directories_grouping_some_paths_become_path_lines() {
        let p = projects_tree();
        let paths: Vec<String> = panel(&p).into_iter().filter(|l| l.starts_with("path")).collect();
        // `/h` and `/h/Projects` hold four of five paths; `/tmp` holds one.
        assert_eq!(
            paths,
            vec!["path /h", "path /h/Projects", "path /h/Projects/_myself", "path /h/Projects/work"]
        );
    }

    #[test]
    fn typing_in_the_panel_narrows_it_and_ctrl_o_keeps_one_path() {
        let mut p = projects_tree();
        press(&mut p, KeyCode::Char('f'), false);
        type_text(&mut p, "_myself");
        assert_eq!(panel(&p), vec!["path /h/Projects/_myself"]);
        press(&mut p, KeyCode::Char('o'), true);
        assert_eq!(shown_ids(&p), vec!["m2", "m1"]);
        assert_eq!(p.hiding().as_deref(), Some("only /h/Projects/_myself"));
        press(&mut p, KeyCode::Esc, false);
        assert!(p.facet_query.is_empty() && p.overlay == Overlay::Filters, "esc clears the query first");
        press(&mut p, KeyCode::Esc, false);
        assert_eq!(p.overlay, Overlay::None);
    }

    #[test]
    fn ctrl_s_keeps_every_matching_project_and_space_hides_a_path() {
        let mut p = projects_tree();
        press(&mut p, KeyCode::Char('f'), false);
        // Fuzzy: n then t, in that order, only in "notes".
        type_text(&mut p, "nt");
        assert_eq!(panel(&p), vec!["project notes"]);
        press(&mut p, KeyCode::Backspace, false);
        press(&mut p, KeyCode::Backspace, false);
        type_text(&mut p, "o");
        let projects: Vec<String> = panel(&p).into_iter().filter(|l| l.starts_with("project")).collect();
        assert_eq!(projects, vec!["project notes", "project blog"], "earliest substring first");
        p.facet_cursor = 0;
        press(&mut p, KeyCode::Char('s'), true);
        assert_eq!(shown_ids(&p), vec!["m2", "m1"]);
        press(&mut p, KeyCode::Char('a'), true);
        press(&mut p, KeyCode::Char('u'), true);
        type_text(&mut p, "/h/projects/work");
        assert_eq!(panel(&p).first().map(String::as_str), Some("path /h/Projects/work"));
        press(&mut p, KeyCode::Char(' '), false);
        assert_eq!(shown_ids(&p), vec!["m2", "m1", "t1"]);
    }

    #[test]
    fn path_filters_survive_a_rebuilt_picker() {
        let mut p = projects_tree();
        p.only_paths.insert("/h/Projects/work".into());
        p.reindex();
        let kept = p.keep();
        let mut q = projects_tree();
        q.restore(kept);
        assert_eq!(shown_ids(&q), vec!["w1", "w2"]);
    }
}
