//! The browser's Remote tab: connect to another host over ssh, then pick OMP
//! sessions on the sending side, live or packed in place, in the same picker
//! `compact` uses (search, harness, project and untitled filters, threshold,
//! sort, checkboxes), and migrate them. ssh's password, passphrase and
//! host-key questions are answered in a dialog.
//!
//! A worker thread owns the connection so the browser keeps drawing, and
//! answers ssh, while ssh connects and archives stream.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction as Axis, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::archive::{Algo, CompressArgs, Compression};
use crate::askpass::{self, Question};
use crate::facts::{Sort, SortKey};
use crate::migrate::{self, Direction, Plan, Source};
use crate::pick::{Picker, Spec};
use crate::transfer::{Inbox, Outbox};
use crate::tui::{centered, ACCENT, BAD, DIM, OK, WARN};
use crate::units::{self, Ctx, Status, Verb};
use crate::wire::{Launch, Remote, SshArgs};
use crate::{clip, human_size, paths};

/// Work the browser hands the connection.
enum Job {
    List(Direction),
    Migrate { direction: Direction, sources: Vec<Source>, delete: bool, relocate: Option<String> },
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
    compression: Compression,
    /// The sessions the picker shows, in the order of its rows.
    plan: Option<Plan>,
    picker: Option<Picker>,
    /// The host dialog: typed text and the offered host under the cursor.
    choosing: Option<(String, usize)>,
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
        let defaults = CompressArgs { algo: None, level: None, threads: None };
        let fallback = Compression { algo: Algo::Zstd, level: 3, threads: 0 };
        RemoteTab {
            hosts: ssh_config_hosts(),
            host: None,
            stage: Stage::Disconnected,
            direction: Direction::Pull,
            compression: defaults.resolve(Algo::Zstd).unwrap_or(fallback),
            plan: None,
            picker: None,
            choosing: None,
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

    /// Opens on the host dialog, preset to pull or push.
    pub fn open(&mut self, direction: Option<Direction>) {
        if let Some(direction) = direction {
            self.direction = direction;
        }
        self.choosing = Some((String::new(), 0));
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

    /// Shows a fresh listing, keeping the user's filters, sort, selection and
    /// settings from the previous one.
    fn show(&mut self, plan: Plan) {
        let rows = plan.sources.iter().map(|s| plan.candidate(s, self.now_ms).row).collect();
        let host = self.host.clone().unwrap_or_default();
        let spec = Spec {
            title: migrate::title(self.direction, &host, self.compression),
            verb: "migrate".into(),
            headers: units::HEADERS.iter().map(|h| h.to_string()).collect(),
            rows,
            threshold_days: Some(0),
            sort: Sort { key: SortKey::Used, reverse: false },
            now_ms: self.now_ms,
            settings: migrate::settings(false, None),
            extra_keys: extra_keys(),
            home: Some(plan.home.clone()),
        };
        let mut picker = Picker::embedded(spec);
        if let Some(old) = &self.picker {
            picker.restore(old.keep());
        }
        self.picker = Some(picker);
        self.plan = Some(plan);
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
        self.picker = None;
    }

    fn send(&mut self, job: Job) {
        let sent = self.stage == Stage::Ready && self.jobs.as_ref().is_some_and(|jobs| jobs.send(job).is_ok());
        if sent {
            self.stage = Stage::Busy;
        } else {
            self.say("busy or not connected: C chooses a host", WARN);
        }
    }

    fn flip_direction(&mut self) {
        self.direction = match self.direction {
            Direction::Pull => Direction::Push,
            Direction::Push => Direction::Pull,
        };
        if self.stage == Stage::Ready {
            self.send(Job::List(self.direction));
        }
    }

    /// Migrates what the picker confirmed.
    fn migrate(&mut self, rows: Vec<usize>) {
        let Some(plan) = &self.plan else { return };
        let sources: Vec<Source> = rows.iter().filter_map(|&i| plan.sources.get(i).cloned()).collect();
        let settings = self.picker.as_ref().map(|p| p.settings().to_vec()).unwrap_or_default();
        let (delete, relocate) = migrate::chosen_settings(&settings);
        if !sources.is_empty() {
            self.send(Job::Migrate { direction: self.direction, sources, delete, relocate });
        }
    }

    /// Handles one key; false leaves it to the browser.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if let Some((question, typed)) = self.question.take() {
            self.answer(question, typed, key.code);
            return true;
        }
        if let Some((text, pick)) = self.choosing.take() {
            self.choose(text, pick, key.code);
            return true;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let consumed = match &mut self.picker {
            Some(picker) if picker.captures_keys() => {
                picker.handle(key);
                true
            }
            _ => false,
        };
        if !consumed {
            match key.code {
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('1' | '2' | '3' | '4' | 'q') => return false,
                KeyCode::Char('c') if ctrl => return false,
                KeyCode::Char('C') => self.choosing = Some((String::new(), 0)),
                KeyCode::Enter if self.picker.is_none() => self.choosing = Some((String::new(), 0)),
                KeyCode::Char('P') => self.flip_direction(),
                KeyCode::Char('r') => self.send(Job::List(self.direction)),
                KeyCode::Char('x') => match &mut self.picker {
                    Some(picker) if self.stage == Stage::Ready => picker.request_confirm(),
                    _ => self.say("busy or not connected", WARN),
                },
                _ => match &mut self.picker {
                    Some(picker) => picker.handle(key),
                    None => return false,
                },
            }
        }
        if let Some(picked) = self.picker.as_mut().and_then(Picker::take_confirmed) {
            self.migrate(picked.rows);
        }
        true
    }

    fn answer(&mut self, question: Question, mut typed: String, code: KeyCode) {
        match code {
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
    }

    fn choose(&mut self, mut text: String, mut pick: usize, code: KeyCode) {
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
        self.choosing = Some((text, pick));
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
        "C connect · P pull/push · x migrate · r relist · tab switch · ? keys · q quit".into()
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let split = Layout::default()
            .direction(Axis::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(4)])
            .split(area);
        frame.render_widget(Paragraph::new(self.state_line()), split[0]);
        let title = migrate::title(self.direction, self.host.as_deref().unwrap_or("no host"), self.compression);
        match &mut self.picker {
            Some(picker) => {
                picker.set_title(title);
                picker.draw(frame, split[1]);
            }
            None => {
                let text = match self.stage {
                    Stage::Disconnected => "  C or enter chooses a host to connect to",
                    _ => "  waiting for the remote host…",
                };
                let block = Block::bordered().border_type(BorderType::Rounded).title(format!(" {title} "));
                frame.render_widget(Paragraph::new(Span::styled(text, Style::default().fg(DIM))).block(block), split[1]);
            }
        }
        if let Some((text, pick)) = &self.choosing {
            self.draw_hosts(frame, area, text, *pick);
        }
        if let Some((question, typed)) = &self.question {
            draw_question(frame, area, question, typed);
        }
    }

    /// Direction, connection state and transfer progress.
    fn state_line(&self) -> Line<'static> {
        let host = self.host.as_deref().unwrap_or("no host");
        let way = match self.direction {
            Direction::Pull => format!(" OMP here ← OMP on {host}"),
            Direction::Push => format!(" OMP here → OMP on {host}"),
        };
        let (stage, color) = match self.stage {
            Stage::Disconnected => ("disconnected", DIM),
            Stage::Connecting => ("connecting…", WARN),
            Stage::Busy => ("migrating…", WARN),
            Stage::Ready => ("connected", OK),
        };
        let mut spans = vec![
            Span::styled(way, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" · {stage}"), Style::default().fg(color)),
        ];
        if let Some((label, done, total)) = &self.progress {
            let percent = done.saturating_mul(100).checked_div(*total).unwrap_or(100);
            spans.push(Span::styled(
                format!(" · {label} {percent}% {} / {}", human_size(*done), human_size(*total)),
                Style::default().fg(ACCENT),
            ));
        }
        Line::from(spans)
    }

    fn draw_hosts(&self, frame: &mut Frame, area: Rect, text: &str, pick: usize) {
        let offered = self.offered(text);
        let area = centered(area, 60, if offered.is_empty() { 20 } else { 60 });
        frame.render_widget(Clear, area);
        let mut lines = vec![Line::from(format!(" {text}▏")), Line::from("")];
        for (n, host) in offered.iter().enumerate().take(20) {
            let style = if n == pick { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default().fg(DIM) };
            lines.push(Line::from(Span::styled(format!("  {host}"), style)));
        }
        lines.push(Line::from(Span::styled(" ↑↓ pick · enter connect · esc cancel", Style::default().fg(DIM))));
        let block = Block::bordered().border_type(BorderType::Rounded).title(" connect to ");
        frame.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
    }
}

/// The browser's keys, for the picker's help.
fn extra_keys() -> Vec<(String, String)> {
    [
        ("C", "connect to a host (Host entries of ~/.ssh/config offered)"),
        ("P", "switch between pull (here ← host) and push (here → host)"),
        ("x / enter", "migrate the selection after a confirmation"),
        ("r", "list the sending side again"),
        ("tab / 1-4", "switch tabs"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
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
            Job::Migrate { direction, sources, delete, relocate } => {
                run(&ctx, &mut remote, direction, &sources, delete, relocate, &events)
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
    sources: &[Source],
    delete: bool,
    relocate: Option<String>,
    events: &Sender<Event>,
) -> anyhow::Result<()> {
    let compression = CompressArgs { algo: None, level: None, threads: None }.resolve(Algo::Zstd)?;
    let settings = migrate::settings(delete, relocate);
    let terms = match migrate::terms(&settings, direction, remote, compression) {
        Ok(terms) => terms,
        Err(err) => {
            let _ = events.send(Event::Log(format!("{err:#}"), BAD));
            return Ok(());
        }
    };
    let mut outbox = Outbox::new(ctx);
    let mut inbox = Inbox::new(ctx);
    let (mut done, mut skipped) = (0, 0);
    for source in sources {
        let (status, _) = migrate::one(remote, direction, source, &terms, &mut inbox, &mut outbox)?;
        let reason = status.reason().map(|r| format!("  ({r})")).unwrap_or_default();
        let color = if status == Status::Done { OK } else { WARN };
        if status.ok() {
            done += 1;
        } else {
            skipped += 1;
        }
        let form = if source.packed() { "packed" } else { "live" };
        let title = clip(if source.title().is_empty() { "(untitled)" } else { source.title() }, 40);
        let line = format!("{:<10} {form:<6} {}  {title}{reason}", status.label(Verb::Migrate), source.id());
        let _ = events.send(Event::Log(line, color));
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
