//! `migrate`: moves OMP sessions between this host and another over SSH, each
//! as a verified archive (a session packed in place travels as it is),
//! restored on the other side, optionally relocated to another directory
//! there, and optionally removed at its source.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde_json::json;

use crate::archive::{Algo, CompressArgs, Compression};
use crate::compact::Local;
use crate::facts::{Facts, Scope, SortArgs, SortKey};
use crate::pick::{Setting, Value};
use crate::transfer::{Inbox, Outbox};
use crate::units::{self, Act, Age, Candidate, Choice, Ctx, Report, Status, Unit, Verb};
use crate::wire::{self, Launch, Remote, SshArgs, Terms};
use crate::{human_size, Side};

const DELETE_KEY: char = 'D';
const RELOCATE_KEY: char = 'R';

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List the OMP sessions on a remote host, live and packed in place.
    List(ListArgs),
    /// Bring OMP sessions from a remote host to this one.
    Pull(MoveArgs),
    /// Send OMP sessions from this host to a remote one.
    Push(MoveArgs),
    /// Answer one migration on stdin and stdout; `pull`, `push` and `list`
    /// start it on the remote host through ssh.
    #[command(hide = true)]
    Serve,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Remote host: an ssh destination, `[user@]host` or a Host from ~/.ssh/config.
    /// Without it the browser opens on its Remote tab.
    pub host: Option<String>,
    #[command(flatten)]
    pub scope: Scope,
    #[command(flatten)]
    pub sort: SortArgs,
    /// List only sessions unused for at least this long: 30d, 2w, 36h, or days.
    #[arg(long)]
    pub older_than: Option<Age>,
    /// Print every detail of each session.
    #[arg(short, long)]
    pub long: bool,
    /// Print JSON instead of a table.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub ssh: SshArgs,
}

#[derive(Debug, Args)]
pub struct MoveArgs {
    /// Remote host: an ssh destination, `[user@]host` or a Host from ~/.ssh/config.
    /// Without it the browser opens on its Remote tab.
    pub host: Option<String>,
    #[command(flatten)]
    pub act: Act,
    #[command(flatten)]
    pub scope: Scope,
    #[command(flatten)]
    pub sort: SortArgs,
    /// Offer only sessions unused for at least this long: 30d, 2w, 36h, or days.
    #[arg(long)]
    pub older_than: Option<Age>,
    /// Remove each session from the host it came from once the other host
    /// has it and every checksum passed.
    #[arg(long)]
    pub delete: bool,
    /// Restore the sessions as run in DIR on the receiving host: their cwd
    /// becomes DIR, and directories recorded under the old cwd move beneath
    /// it. DIR must exist there; `~` is that host's home.
    #[arg(long, value_name = "DIR")]
    pub relocate: Option<String>,
    #[command(flatten)]
    pub compress: CompressArgs,
    #[command(flatten)]
    pub ssh: SshArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Pull,
    Push,
}

/// True for the commands that stream archives: a closed pipe must surface as
/// an error they clean up after, not end the process.
pub fn streams(command: &Command) -> bool {
    !matches!(command, Command::List(_))
}

pub fn run(ctx: &Ctx, command: Command) -> Result<i32> {
    match command {
        Command::List(args) => match args.host.clone() {
            Some(host) => list(ctx, &args, &host),
            None => browse(None),
        },
        Command::Pull(args) => match args.host.clone() {
            Some(host) => migrate(ctx, &args, &host, Direction::Pull),
            None => browse(Some(Direction::Pull)),
        },
        Command::Push(args) => match args.host.clone() {
            Some(host) => migrate(ctx, &args, &host, Direction::Push),
            None => browse(Some(Direction::Push)),
        },
        Command::Serve => wire::serve(ctx),
    }
}

/// The browser on its Remote tab, asking for a host; a terminal is required.
pub fn browse(direction: Option<Direction>) -> Result<i32> {
    use std::io::IsTerminal;
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        bail!("name a host: without one, migrate opens the browser, which needs a terminal");
    }
    crate::tui::run_remote(direction)?;
    Ok(0)
}

fn only_omp(scope: &Scope) -> Result<()> {
    if !scope.sides().contains(&Side::Pi) {
        bail!("migrate moves OMP sessions only, and the filters leave them out");
    }
    Ok(())
}

/// One session on the sending host: live, or packed in place.
#[derive(Debug, Clone)]
pub enum Source {
    Live(Box<Unit>),
    Packed(Box<Local>),
}

impl Source {
    pub fn id(&self) -> &str {
        match self {
            Source::Live(unit) => &unit.id,
            Source::Packed(local) => &local.manifest.id,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Source::Live(unit) => &unit.title,
            Source::Packed(local) => &local.manifest.title,
        }
    }

    pub fn packed(&self) -> bool {
        matches!(self, Source::Packed(_))
    }

    /// Size of the session itself, before any packing.
    pub fn bytes(&self) -> u64 {
        match self {
            Source::Live(unit) => unit.bytes,
            Source::Packed(local) => local.manifest.bytes,
        }
    }

    fn facts(&self) -> Facts {
        match self {
            Source::Live(unit) => units::unit_facts(unit),
            Source::Packed(local) => units::manifest_facts(&local.manifest, local.archive_bytes),
        }
    }
}

/// Live sessions first, then sessions packed in place.
fn sources(units: Vec<Unit>, packed: Vec<Local>) -> Vec<Source> {
    let live = units.into_iter().map(|u| Source::Live(Box::new(u)));
    live.chain(packed.into_iter().map(|l| Source::Packed(Box::new(l)))).collect()
}

fn list(ctx: &Ctx, args: &ListArgs, host: &str) -> Result<i32> {
    only_omp(&args.scope)?;
    let mut remote = Remote::connect(host, &args.ssh, Launch::default())?;
    let listing = remote.list()?;
    let (host, root) = (remote.host.clone(), PathBuf::from(&remote.pi_root));
    let scope = args.scope.on_host(&remote.home);
    drop(remote);
    let here = Inbox::new(ctx).present().clone();
    let all = sources(listing.units, listing.packed);
    let old_enough = |f: &Facts| args.older_than.is_none_or(|age| units::age_days(ctx.now_ms, f.last_used_ms) >= age.days);
    let mut shown: Vec<(Facts, &Source)> =
        all.iter().map(|s| (s.facts(), s)).filter(|(f, _)| scope.admits(f) && old_enough(f)).collect();
    let sort = args.sort.resolve(SortKey::Used);
    shown.sort_by(|(a, _), (b, _)| sort.compare(a, b));
    let held = &listing.held;
    if args.json {
        let rows: Vec<_> = shown
            .iter()
            .map(|(_, s)| {
                let open = !s.packed() && held.contains(s.id());
                let session = match s {
                    Source::Live(unit) => json!({"unit": unit}),
                    Source::Packed(local) => json!({"packed": local}),
                };
                json!({"session": session, "open": open, "here": here.contains(s.id())})
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if !args.long {
        units::print_header();
    }
    for (facts, source) in &shown {
        if args.long {
            println!("── {} ──", if source.title().is_empty() { source.id() } else { source.title() });
            let mut details = row(source, &root, ctx.now_ms).details;
            details.push(("state".into(), state(source, held, &here, &host)));
            units::print_details(&details);
            println!();
        } else {
            units::print_line(facts, ctx.now_ms);
        }
    }
    let bytes: u64 = shown.iter().map(|(_, s)| s.bytes()).sum();
    let packed = shown.iter().filter(|(_, s)| s.packed()).count();
    let open = shown.iter().filter(|(_, s)| !s.packed() && held.contains(s.id())).count();
    let present = shown.iter().filter(|(_, s)| here.contains(s.id())).count();
    println!(
        "{} OMP session(s) on {host} ({packed} packed in place) · {} · {open} open there · {present} already here",
        shown.len(),
        human_size(bytes),
    );
    Ok(0)
}

fn state(source: &Source, held: &HashSet<String>, here: &HashSet<String>, host: &str) -> String {
    if !source.packed() && held.contains(source.id()) {
        format!("open in a running omp on {host}")
    } else if here.contains(source.id()) {
        "already here".into()
    } else {
        "can migrate".into()
    }
}

/// Picker row of a session on the host whose Pi root is `root`.
fn row(source: &Source, root: &std::path::Path, now_ms: i64) -> crate::pick::Row {
    match source {
        Source::Live(unit) => units::unit_row(unit, root, now_ms),
        Source::Packed(local) => units::archive_row(&local.manifest, local.archive_bytes, &local.path, now_ms),
    }
}

/// The sessions on the sending host, and what blocks them.
pub struct Plan {
    pub sources: Vec<Source>,
    pub root: PathBuf,
    held: HashSet<String>,
    /// Ids the receiving host already has, live or packed.
    present: HashSet<String>,
    pub from: String,
    pub to: String,
    /// Home of the sending host, which its sessions' paths sit under.
    pub home: String,
}

pub fn plan(ctx: &Ctx, remote: &mut Remote, outbox: &mut Outbox, inbox: &mut Inbox, direction: Direction) -> Result<Plan> {
    let listing = remote.list()?;
    let host = remote.host.clone();
    Ok(match direction {
        Direction::Pull => Plan {
            sources: sources(listing.units, listing.packed),
            root: PathBuf::from(&remote.pi_root),
            held: listing.held,
            present: inbox.present().clone(),
            from: host,
            to: "this host".into(),
            home: remote.home.clone(),
        },
        Direction::Push => {
            let units = outbox.units().to_vec();
            let ids: Vec<String> = units.iter().map(|u| u.id.clone()).collect();
            let present = listing.units.into_iter().map(|u| u.id);
            Plan {
                held: outbox.held(&ids),
                sources: sources(units, outbox.packed().to_vec()),
                root: ctx.roots.pi.clone(),
                present: present.chain(listing.packed.into_iter().map(|l| l.manifest.id)).collect(),
                from: "this host".into(),
                to: host,
                home: crate::paths::home().display().to_string(),
            }
        }
    })
}

impl Plan {
    pub fn blocked(&self, source: &Source) -> Option<Status> {
        if !source.packed() && self.held.contains(source.id()) {
            Some(Status::Held)
        } else if self.present.contains(source.id()) {
            Some(Status::Conflict(format!("already on {}", self.to)))
        } else {
            None
        }
    }

    pub fn candidate(&self, source: &Source, now_ms: i64) -> Candidate {
        let mut row = row(source, &self.root, now_ms);
        let form = if source.packed() { "packed in place" } else { "live" };
        let lands = if source.packed() { "kept packed" } else { "restored" };
        row.details.insert(0, ("host".into(), format!("{} · {form}", self.from)));
        row.details.insert(1, ("migrates".into(), format!("OMP → OMP on {}, {lands} in its cwd's bucket", self.to)));
        row.blocked = match self.blocked(source) {
            Some(Status::Held) => Some(format!("open in a running omp on {}", self.from)),
            Some(_) => Some(format!("already on {}", self.to)),
            None => None,
        };
        let path = match source {
            Source::Packed(local) if self.from == "this host" => Some(local.path.clone()),
            _ => None,
        };
        Candidate { path, row }
    }
}

/// Delete-source and relocate-to, as the picker shows them.
pub fn settings(delete: bool, relocate: Option<String>) -> Vec<Setting> {
    vec![
        Setting { key: DELETE_KEY, label: "delete source".into(), value: Value::Flag(delete), warn: true },
        Setting {
            key: RELOCATE_KEY,
            label: "relocate to".into(),
            value: Value::Text(relocate.unwrap_or_default()),
            warn: false,
        },
    ]
}

/// The delete flag and the relocation directory, as the picker left them.
pub fn chosen_settings(settings: &[Setting]) -> (bool, Option<String>) {
    let mut delete = false;
    let mut relocate = None;
    for setting in settings {
        match (setting.key, &setting.value) {
            (DELETE_KEY, Value::Flag(on)) => delete = *on,
            (RELOCATE_KEY, Value::Text(dir)) if !dir.trim().is_empty() => relocate = Some(dir.trim().to_string()),
            _ => {}
        }
    }
    (delete, relocate)
}

/// The picker title for one direction.
pub fn title(direction: Direction, host: &str, compression: Compression) -> String {
    match direction {
        Direction::Pull => format!("migrate OMP ← {host} · {compression}"),
        Direction::Push => format!("migrate OMP → {host} · {compression}"),
    }
}

fn migrate(ctx: &Ctx, args: &MoveArgs, host: &str, direction: Direction) -> Result<i32> {
    only_omp(&args.scope)?;
    let compression = args.compress.resolve(Algo::Zstd)?;
    let mut remote = Remote::connect(host, &args.ssh, Launch::default())?;
    let mut outbox = Outbox::new(ctx);
    let mut inbox = Inbox::new(ctx);
    let plan = plan(ctx, &mut remote, &mut outbox, &mut inbox, direction)?;

    let candidates = plan.sources.iter().map(|s| plan.candidate(s, ctx.now_ms)).collect();
    let choice = Choice {
        title: title(direction, &remote.host, compression),
        verb: "migrate",
        threshold: Some(args.older_than.unwrap_or(Age { days: 0.0 })),
        sort: args.sort.resolve(SortKey::Used),
        now_ms: ctx.now_ms,
    };
    let mut settings = settings(args.delete, args.relocate.clone());
    // Path filters name directories of the sending host.
    let remote_scope = args.scope.on_host(&remote.home);
    let scope = if direction == Direction::Pull { &remote_scope } else { &args.scope };
    let chosen = units::choose_with(candidates, &args.act, scope, choice, &mut settings)?;
    if chosen.is_empty() {
        println!("nothing chosen");
        return Ok(0);
    }
    let terms = terms(&settings, direction, &mut remote, compression)?;
    println!("{}", describe(&plan, &terms));

    let mut report = Report::new(Verb::Migrate, args.act.dry_run);
    for i in chosen {
        let source = &plan.sources[i];
        let mut sizes = human_size(source.bytes());
        let status = if let Some(status) = plan.blocked(source) {
            status
        } else if args.act.dry_run {
            Status::Would
        } else {
            let (status, archive) = one(&mut remote, direction, source, &terms, &mut inbox, &mut outbox)?;
            if archive > 0 {
                sizes = format!("{} → {}", human_size(source.bytes()), human_size(archive));
            }
            status
        };
        if status == Status::Done || status == Status::Would {
            match direction {
                Direction::Pull => report.restored += source.bytes(),
                Direction::Push if terms.delete => report.freed += source.bytes(),
                Direction::Push => {}
            }
        }
        report.line(&status, Side::Pi, source.id(), source.title(), &sizes);
    }
    Ok(report.finish())
}

/// The terms the picker or the flags left, with the relocation target
/// resolved on the receiving host.
pub fn terms(settings: &[Setting], direction: Direction, remote: &mut Remote, compression: Compression) -> Result<Terms> {
    let (delete, relocate) = chosen_settings(settings);
    let relocate = relocate
        .map(|dir| match direction {
            Direction::Pull => Inbox::target(&dir),
            Direction::Push => remote.target(&dir),
        })
        .transpose()?;
    Ok(Terms { compression, relocate, delete })
}

/// Migrates one session; an `Err` broke the connection.
pub fn one(
    remote: &mut Remote,
    direction: Direction,
    source: &Source,
    terms: &Terms,
    inbox: &mut Inbox,
    outbox: &mut Outbox,
) -> Result<(Status, u64)> {
    match direction {
        Direction::Pull => remote.fetch(source.id(), source.packed(), terms, inbox),
        Direction::Push => remote.deliver(source.id(), source.packed(), terms, outbox),
    }
}

fn describe(plan: &Plan, terms: &Terms) -> String {
    let mut line = format!("OMP {} → OMP {} · {}", plan.from, plan.to, terms.compression);
    if let Some(dir) = &terms.relocate {
        line.push_str(&format!(" · relocate to {dir}"));
    }
    if terms.delete {
        line.push_str(" · delete source");
    }
    line
}
