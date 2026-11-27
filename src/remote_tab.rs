//! The browser's Remote tab: connect to another host over ssh, list the OMP
//! sessions on the sending side, select them and migrate them, with ssh's
//! password, passphrase and host-key questions answered in a dialog.
//!
//! A worker thread owns the connection so the browser keeps drawing, and
//! answers ssh, while ssh connects and archives stream.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Direction as Axis, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::archive::{Algo, CompressArgs};
use crate::askpass::{self, Question};
use crate::migrate::{self, Direction, Plan};
use crate::pick::Row;
use crate::transfer::{Inbox, Outbox};
use crate::tui::{centered, ACCENT, BAD, DIM, OK, WARN};
use crate::units::{Ctx, Status, Verb};
use crate::wire::{Launch, Remote, SshArgs, Terms};
use crate::{clip, human_size, paths};

/// Work the browser hands the connection.
enum Job {
    List(Direction),
    Migrate { direction: Direction, ids: Vec<String>, delete: bool, relocate: Option<String> },
}

/// What the connection reports back.
enum Event {
    Connected { host: String },
    Listed(Plan),
    Log(String, Color),
    Progress(String, u64, u64),
    /// A job finished; the connection stays.
    Idle,
    /// The connection is gone, or never came up.
    Closed(String),
}

/// Text typed into a dialog.
enum Input {
    Host { text: String, pick: usize },
    Relocate(String),
    Filter(String),
}

#[derive(PartialEq, Eq)]
enum Stage {
    Disconnected,
    Connecting,
    Ready,
    Busy,
}

pub struct RemoteTab {
    /// `Host` entries of `~/.ssh/config`, offered when choosing a host.
    hosts: Vec<String>,
    host: Option<String>,
    stage: Stage,
    direction: Direction,
    plan: Option<Plan>,
    rows: Vec<Row>,
    /// Indices into `rows` the filter shows.
    view: Vec<usize>,
    filter: String,
    cursor: usize,
    selected: HashSet<String>,
    delete: bool,
    relocate: String,
    input: Option<Input>,
    confirm: bool,
    question: Option<(Question, String)>,
    progress: Option<(String, u64, u64)>,
    jobs: Option<Sender<Job>>,
    events_tx: Sender<Event>,
    events: Receiver<Event>,
    questions_tx: Sender<Question>,
    questions: Receiver<Question>,
    askpass: Option<askpass::Server>,
    /// Lines for the browser's activity log.
    pub log: Vec<(String, Color)>,
    now_ms: i64,
}

impl RemoteTab {
    pub fn new() -> RemoteTab {
        let (events_tx, events) = mpsc::channel();
        let (questions_tx, questions) = mpsc::channel();
        RemoteTab {
            hosts: ssh_config_hosts(),
            host: None,
            stage: Stage::Disconnected,
            direction: Direction::Pull,
            plan: None,
            rows: Vec::new(),
            view: Vec::new(),
            filter: String::new(),
            cursor: 0,
            selected: HashSet::new(),
            delete: false,
            relocate: String::new(),
            input: None,
            confirm: false,
            question: None,
            progress: None,
            jobs: None,
            events_tx,
            events,
            questions_tx,
            questions,
            askpass: None,
            log: Vec::new(),
            now_ms: chrono::Utc::now().timestamp_millis(),
        }
    }

    /// True while ssh waits for an answer, which the browser must show.
    pub fn asking(&self) -> bool {
        self.question.is_some()
    }

    fn say(&mut self, text: impl Into<String>, color: Color) {
        self.log.push((text.into(), color));
    }

    /// Takes in what the connection and ssh sent since the last frame.
    pub fn poll(&mut self) {
        while let Ok(question) = self.questions.try_recv() {
            // A newer question replaces one ssh gave up on.
            if let Some((old, _)) = self.question.take() {
                old.answer(None);
            }
            self.question = Some((question, String::new()));
        }
        while let Ok(event) = self.events.try_recv() {
            match event {
                Event::Connected { host } => {
                    self.say(format!("connected to {host}"), OK);
                    self.stage = Stage::Ready;
                }
                Event::Listed(plan) => self.show(plan),
                Event::Log(text, color) => self.say(text, color),
                Event::Progress(label, done, total) => self.progress = Some((label, done, total)),
                Event::Idle => {
                    self.progress = None;
                    if self.stage == Stage::Busy {
                        self.stage = Stage::Ready;
                    }
                }
                Event::Closed(why) => {
                    self.say(why, BAD);
                    self.stage = Stage::Disconnected;
                    self.jobs = None;
                    self.progress = None;
                    if let Some((question, _)) = self.question.take() {
                        question.answer(None);
                    }
                }
            }
        }
    }

    fn show(&mut self, plan: Plan) {
        self.rows = plan.sources.iter().map(|u| plan.candidate(u, self.now_ms).row).collect();
        let ids: HashSet<&str> = plan.sources.iter().map(|u| u.id.as_str()).collect();
        self.selected.retain(|id| ids.contains(id.as_str()));
        self.plan = Some(plan);
        self.reindex();
    }

    fn reindex(&mut self) {
        let needle = self.filter.to_lowercase();
        let mut view: Vec<usize> = (0..self.rows.len()).filter(|&i| self.rows[i].facts.mentions(&needle)).collect();
        view.sort_by_key(|&i| self.rows[i].facts.last_used_ms);
        self.view = view;
        self.cursor = self.cursor.min(self.view.len().saturating_sub(1));
    }

    fn connect(&mut self, host: String) {
        if self.askpass.is_none() {
            match askpass::Server::start(self.questions_tx.clone()) {
                Ok(server) => self.askpass = Some(server),
                Err(err) => {
                    self.say(format!("cannot ask for passwords here: {err:#}"), BAD);
                    return;
                }
            }
        }
        let socket = self.askpass.as_ref().map(|s| s.socket.clone());
        let (jobs, inbox) = mpsc::channel();
        let events = self.events_tx.clone();
        let direction = self.direction;
        let target = host.clone();
        thread::spawn(move || worker(target, socket, direction, inbox, events));
        self.say(format!("connecting to {host}…"), ACCENT);
        self.jobs = Some(jobs);
        self.host = Some(host);
        self.stage = Stage::Connecting;
        self.plan = None;
        self.rows.clear();
        self.view.clear();
        self.selected.clear();
    }

    fn send(&mut self, job: Job) {
        let sent = self.jobs.as_ref().is_some_and(|jobs| jobs.send(job).is_ok());
        if sent {
            self.stage = Stage::Busy;
        } else {
            self.say("not connected: c chooses a host", WARN);
        }
    }

    fn flip_direction(&mut self) {
        self.direction = match self.direction {
            Direction::Pull => Direction::Push,
            Direction::Push => Direction::Pull,
        };
        self.selected.clear();
        if self.stage == Stage::Ready {
            self.send(Job::List(self.direction));
        }
    }

    fn cursor_row(&self) -> Option<usize> {
        self.view.get(self.cursor).copied()
    }

    fn toggle(&mut self, index: usize) {
        if let Some(reason) = self.rows[index].blocked.clone() {
            self.say(reason, WARN);
            return;
        }
        let id = self.rows[index].facts.id.clone();
        if !self.selected.remove(&id) {
            self.selected.insert(id);
        }
    }

    fn toggle_all(&mut self) {
        let shown: Vec<String> =
            self.view.iter().filter(|&&i| self.rows[i].blocked.is_none()).map(|&i| self.rows[i].facts.id.clone()).collect();
        let all = !shown.is_empty() && shown.iter().all(|id| self.selected.contains(id));
        for id in shown {
            if all {
                self.selected.remove(&id);
            } else {
                self.selected.insert(id);
            }
        }
    }

    /// Selected sessions still shown, else the one under the cursor.
    fn chosen(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .view
            .iter()
            .map(|&i| &self.rows[i])
            .filter(|r| r.blocked.is_none() && self.selected.contains(&r.facts.id))
            .map(|r| r.facts.id.clone())
            .collect();
        if ids.is_empty() {
            if let Some(i) = self.cursor_row().filter(|&i| self.rows[i].blocked.is_none()) {
                ids.push(self.rows[i].facts.id.clone());
            }
        }
        ids
    }

    fn start_migration(&mut self) {
        let ids = self.chosen();
        if ids.is_empty() {
            return;
        }
        let relocate = Some(self.relocate.trim().to_string()).filter(|d| !d.is_empty());
        self.send(Job::Migrate { direction: self.direction, ids, delete: self.delete, relocate });
    }

    /// Handles one key; false leaves it to the browser.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if let Some((question, mut typed)) = self.question.take() {
            match key.code {
                KeyCode::Enter if question.notice => question.answer(None),
                KeyCode::Enter => question.answer(Some(typed)),
                KeyCode::Esc => {
                    question.answer(None);
                    self.say("declined ssh's question", WARN);
                }
                KeyCode::Backspace => {
                    typed.pop();
                    self.question = Some((question, typed));
                }
                KeyCode::Char(c) if !question.notice => {
                    typed.push(c);
                    self.question = Some((question, typed));
                }
                _ => self.question = Some((question, typed)),
            }
            return true;
        }
        if self.confirm {
            self.confirm = false;
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Enter) {
                self.start_migration();
            }
            return true;
        }
        if let Some(input) = self.input.take() {
            self.edit(input, key.code);
            return true;
        }
        let last = self.view.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('c') => self.input = Some(Input::Host { text: String::new(), pick: 0 }),
            KeyCode::Enter if self.stage == Stage::Disconnected => {
                self.input = Some(Input::Host { text: String::new(), pick: 0 })
            }
            KeyCode::Char('p') => self.flip_direction(),
            KeyCode::Char('r') if self.stage == Stage::Ready => self.send(Job::List(self.direction)),
            KeyCode::Char('j') | KeyCode::Down => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::PageDown => self.cursor = (self.cursor + 10).min(last),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => self.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.cursor = last,
            KeyCode::Char(' ') => {
                if let Some(i) = self.cursor_row() {
                    self.toggle(i);
                }
            }
            KeyCode::Char('a') => self.toggle_all(),
            KeyCode::Char('A') => self.selected.clear(),
            KeyCode::Char('/') => self.input = Some(Input::Filter(self.filter.clone())),
            KeyCode::Char('D') => {
                self.delete = !self.delete;
                let state = if self.delete { "on" } else { "off" };
                self.say(format!("delete source: {state}"), WARN);
            }
            KeyCode::Char('R') => self.input = Some(Input::Relocate(self.relocate.clone())),
            KeyCode::Char('x') if self.stage == Stage::Ready => {
                if self.chosen().is_empty() {
                    self.say("nothing selected to migrate", WARN);
                } else {
                    self.confirm = true;
                }
            }
            KeyCode::Char('x') => self.say("busy or not connected", WARN),
            _ => return false,
        }
        true
    }

    fn edit(&mut self, input: Input, code: KeyCode) {
        match input {
            Input::Host { mut text, mut pick } => {
                let offered = self.offered(&text);
                match code {
                    KeyCode::Esc => return,
                    KeyCode::Enter => {
                        let host = offered.get(pick).cloned().unwrap_or_else(|| text.trim().to_string());
                        if !host.is_empty() {
                            self.connect(host);
                        }
                        return;
                    }
                    KeyCode::Down => pick = (pick + 1).min(offered.len().saturating_sub(1)),
                    KeyCode::Up => pick = pick.saturating_sub(1),
                    KeyCode::Backspace => {
                        text.pop();
                        pick = 0;
                    }
                    KeyCode::Char(c) => {
                        text.push(c);
                        pick = 0;
                    }
                    _ => {}
                }
                self.input = Some(Input::Host { text, pick });
            }
            Input::Relocate(mut text) => match code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.relocate = text.trim().to_string(),
                KeyCode::Backspace => {
                    text.pop();
                    self.input = Some(Input::Relocate(text));
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    self.input = Some(Input::Relocate(text));
                }
                _ => self.input = Some(Input::Relocate(text)),
            },
            Input::Filter(mut text) => {
                match code {
                    KeyCode::Esc => text.clear(),
                    KeyCode::Enter => {}
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) => {
                        text.push(c);
                    }
                    _ => {}
                }
                let done = matches!(code, KeyCode::Esc | KeyCode::Enter);
                self.filter = text.clone();
                self.reindex();
                if !done {
                    self.input = Some(Input::Filter(text));
                }
            }
        }
    }

    /// Configured hosts matching what was typed; the typed text itself is
    /// used when none matches.
    fn offered(&self, typed: &str) -> Vec<String> {
        let needle = typed.trim().to_lowercase();
        self.hosts.iter().filter(|h| h.to_lowercase().contains(&needle)).cloned().collect()
    }

    pub fn tab_title(&self) -> String {
        match &self.host {
            Some(host) => format!(" Remote · {} ", clip(host, 20)),
            None => " Remote ".to_string(),
        }
    }

    pub fn footer(&self) -> String {
        if let Some(Input::Filter(text)) = &self.input {
            return format!("filter: {text}▏ (enter to apply, esc to clear)");
        }
        "c connect · p pull/push · space select · a all · / filter · D delete source · R relocate · x migrate · r refresh · tab switch · ? help · q quit".into()
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let split = Layout::default()
            .direction(Axis::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area);
        self.draw_list(frame, split[0]);
        self.draw_detail(frame, split[1]);
        if let Some(input) = &self.input {
            self.draw_input(frame, area, input);
        }
        if self.confirm {
            self.draw_confirm(frame, area);
        }
        if let Some((question, typed)) = &self.question {
            draw_question(frame, area, question, typed);
        }
    }

    fn heading(&self) -> String {
        let host = self.host.as_deref().unwrap_or("no host");
        let way = match self.direction {
            Direction::Pull => format!("pull ← {host}"),
            Direction::Push => format!("push → {host}"),
        };
        let stage = match self.stage {
            Stage::Disconnected => " · disconnected",
            Stage::Connecting => " · connecting…",
            Stage::Busy => " · working…",
            Stage::Ready => "",
        };
        format!(" {way}{stage} ")
    }

    fn draw_list(&self, frame: &mut Frame, area: Rect) {
        let mut settings = vec![Span::styled(
            format!(" delete source: {} ", if self.delete { "on" } else { "off" }),
            Style::default().fg(if self.delete { WARN } else { DIM }),
        )];
        let relocate = if self.relocate.is_empty() { "—" } else { self.relocate.as_str() };
        settings.push(Span::styled(format!("· relocate to: {} ", clip(relocate, 40)), Style::default().fg(DIM)));
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(Span::styled(self.heading(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)))
            .title_bottom(Line::from(settings));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines: Vec<ListItem> = Vec::new();
        if self.host.is_none() {
            lines.push(ListItem::new(Span::styled("  c or enter chooses a host to connect to", Style::default().fg(DIM))));
        } else if self.rows.is_empty() && self.stage != Stage::Ready {
            lines.push(ListItem::new(Span::styled("  waiting for the remote host…", Style::default().fg(DIM))));
        } else if self.view.is_empty() {
            lines.push(ListItem::new(Span::styled("  no OMP sessions on the sending side", Style::default().fg(DIM))));
        }
        for (pos, &i) in self.view.iter().enumerate() {
            let row = &self.rows[i];
            let mark = if row.blocked.is_some() {
                "·  "
            } else if self.selected.contains(&row.facts.id) {
                "[x]"
            } else {
                "[ ]"
            };
            let style = match (row.blocked.is_some(), pos == self.cursor, self.selected.contains(&row.facts.id)) {
                (true, _, _) => Style::default().fg(DIM),
                (_, true, _) => Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                (_, _, true) => Style::default().fg(OK),
                _ => Style::default(),
            };
            let c = &row.columns;
            let text = format!(
                "{mark} {:>5} {:>9}  {:<18} {}",
                c.first().map_or("", String::as_str),
                c.get(1).map_or("", String::as_str),
                clip(c.get(4).map_or("", String::as_str), 18),
                c.get(5).map_or("", String::as_str)
            );
            lines.push(ListItem::new(Span::styled(text, style)));
        }
        let mut state = ListState::default();
        if !self.view.is_empty() {
            state.select(Some(self.cursor));
        }
        frame.render_stateful_widget(List::new(lines), inner, &mut state);
    }

    fn draw_detail(&self, frame: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        if let Some((label, done, total)) = &self.progress {
            let percent = done.saturating_mul(100).checked_div(*total).unwrap_or(100);
            lines.push(Line::from(Span::styled(
                format!("{label}  {percent}%  {} / {}", human_size(*done), human_size(*total)),
                Style::default().fg(ACCENT),
            )));
            lines.push(Line::from(""));
        }
        if let Some(plan) = &self.plan {
            lines.push(Line::from(Span::styled(format!("{} → {}", plan.from, plan.to), Style::default().fg(DIM))));
        }
        if let Some(i) = self.cursor_row() {
            let row = &self.rows[i];
            let width = row.details.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0) + 2;
            if let Some(reason) = &row.blocked {
                lines.push(Line::from(Span::styled(reason.clone(), Style::default().fg(WARN))));
            }
            for (label, value) in &row.details {
                for (n, part) in value.split('\n').enumerate() {
                    let label = if n == 0 { label.as_str() } else { "" };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{label:<width$}"), Style::default().fg(DIM)),
                        Span::raw(part.to_string()),
                    ]));
                }
            }
        }
        let paragraph = Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(Block::bordered().border_type(BorderType::Rounded).title(" detail "));
        frame.render_widget(paragraph, area);
    }

    fn draw_input(&self, frame: &mut Frame, area: Rect, input: &Input) {
        let (title, text, offered, pick) = match input {
            Input::Host { text, pick } => (" connect to ", text.as_str(), self.offered(text), *pick),
            Input::Relocate(text) => (" relocate to (directory on the receiving host; empty for none) ", text.as_str(), Vec::new(), 0),
            Input::Filter(_) => return,
        };
        let area = centered(area, 60, if offered.is_empty() { 20 } else { 60 });
        frame.render_widget(Clear, area);
        let mut lines = vec![Line::from(format!(" {text}▏")), Line::from("")];
        for (n, host) in offered.iter().enumerate().take(20) {
            let style = if n == pick { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default().fg(DIM) };
            lines.push(Line::from(Span::styled(format!("  {host}"), style)));
        }
        lines.push(Line::from(Span::styled(" enter accept · esc cancel", Style::default().fg(DIM))));
        frame.render_widget(
            Paragraph::new(Text::from(lines)).block(Block::bordered().border_type(BorderType::Rounded).title(title)),
            area,
        );
    }

    fn draw_confirm(&self, frame: &mut Frame, area: Rect) {
        let area = centered(area, 60, 45);
        frame.render_widget(Clear, area);
        let ids = self.chosen();
        let bytes: u64 =
            self.rows.iter().filter(|r| ids.contains(&r.facts.id)).map(|r| r.facts.bytes).sum();
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("{} {} session(s) · {}?", self.heading().trim(), ids.len(), human_size(bytes)),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        if self.delete {
            lines.push(Line::from(Span::styled("delete source: on", Style::default().fg(WARN))));
        }
        if !self.relocate.is_empty() {
            lines.push(Line::from(format!("relocate to: {}", self.relocate)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("y / enter confirm · n / esc back", Style::default().fg(DIM))));
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .alignment(ratatui::layout::Alignment::Center)
                .wrap(Wrap { trim: true })
                .block(Block::bordered().border_type(BorderType::Rounded).title(" migrate ")),
            area,
        );
    }
}

fn draw_question(frame: &mut Frame, area: Rect, question: &Question, typed: &str) {
    let area = centered(area, 70, 40);
    frame.render_widget(Clear, area);
    let shown = if question.secret { "•".repeat(typed.chars().count()) } else { typed.to_string() };
    let mut lines: Vec<Line> = question.prompt.lines().map(|l| Line::from(l.to_string())).collect();
    lines.push(Line::from(""));
    if question.notice {
        lines.push(Line::from(Span::styled("enter dismiss", Style::default().fg(DIM))));
    } else {
        lines.push(Line::from(Span::styled(format!("> {shown}▏"), Style::default().fg(ACCENT))));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("enter answer · esc decline", Style::default().fg(DIM))));
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(Block::bordered().border_type(BorderType::Rounded).title(" ssh asks ")),
        area,
    );
}

/// Owns one connection: connects, then runs jobs until the browser drops
/// its end or the connection breaks.
fn worker(host: String, socket: Option<PathBuf>, direction: Direction, jobs: Receiver<Job>, events: Sender<Event>) {
    let ctx = Ctx::from_env();
    let lines = events.clone();
    let progress = events.clone();
    let launch = Launch {
        askpass: socket,
        stderr: Some(Box::new(move |line| {
            let _ = lines.send(Event::Log(format!("ssh: {line}"), DIM));
        })),
        progress: Some(Box::new(move |label, done, total| {
            let _ = progress.send(Event::Progress(label.to_string(), done, total));
        })),
    };
    let mut remote = match Remote::connect(&host, &SshArgs::default(), launch) {
        Ok(remote) => remote,
        Err(err) => {
            let _ = events.send(Event::Closed(format!("{err:#}")));
            return;
        }
    };
    let _ = events.send(Event::Connected { host: host.clone() });
    let mut first = Some(Job::List(direction));
    while let Some(job) = first.take().or_else(|| jobs.recv().ok()) {
        let result = match job {
            Job::List(direction) => list(&ctx, &mut remote, direction, &events),
            Job::Migrate { direction, ids, delete, relocate } => {
                run(&ctx, &mut remote, direction, &ids, delete, relocate, &events)
                    .and_then(|()| list(&ctx, &mut remote, direction, &events))
            }
        };
        if let Err(err) = result {
            let _ = events.send(Event::Closed(format!("connection to {host} lost: {err:#}")));
            return;
        }
        let _ = events.send(Event::Idle);
    }
}

fn list(ctx: &Ctx, remote: &mut Remote, direction: Direction, events: &Sender<Event>) -> anyhow::Result<()> {
    let plan = migrate::plan(ctx, remote, &mut Outbox::new(ctx), &mut Inbox::new(ctx), direction)?;
    let _ = events.send(Event::Listed(plan));
    Ok(())
}

fn run(
    ctx: &Ctx,
    remote: &mut Remote,
    direction: Direction,
    ids: &[String],
    delete: bool,
    relocate: Option<String>,
    events: &Sender<Event>,
) -> anyhow::Result<()> {
    let compression = CompressArgs { algo: None, level: None, threads: None }.resolve(Algo::Zstd)?;
    let relocate = match relocate.map(|dir| migrate::target(&dir, direction, remote)).transpose() {
        Ok(relocate) => relocate,
        Err(err) => {
            let _ = events.send(Event::Log(format!("{err:#}"), BAD));
            return Ok(());
        }
    };
    let terms = Terms { compression, relocate, delete };
    let mut outbox = Outbox::new(ctx);
    let mut inbox = Inbox::new(ctx);
    let (mut done, mut skipped) = (0, 0);
    for id in ids {
        let (status, _) = migrate::one(remote, direction, id, &terms, &mut inbox, &mut outbox)?;
        let reason = status.reason().map(|r| format!("  ({r})")).unwrap_or_default();
        let color = if status == Status::Done { OK } else { WARN };
        if status.ok() {
            done += 1;
        } else {
            skipped += 1;
        }
        let _ = events.send(Event::Log(format!("{:<10} {id}{reason}", status.label(Verb::Migrate)), color));
    }
    let _ = events.send(Event::Log(format!("{done} migrated · {skipped} skipped"), ACCENT));
    Ok(())
}

/// Concrete `Host` names in `~/.ssh/config`, in file order.
fn ssh_config_hosts() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(paths::home().join(".ssh/config")) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if !words.next().is_some_and(|w| w.eq_ignore_ascii_case("host")) {
            continue;
        }
        for name in words {
            if !name.contains(['*', '?', '!']) && !out.iter().any(|h| h == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}
