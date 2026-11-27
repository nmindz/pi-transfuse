//! `migrate`: moves OMP sessions between this host and another over SSH, each
//! packed into a verified archive, restored on the other side, optionally
//! relocated to another directory there, and optionally removed at its source.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use serde_json::json;

use crate::archive::{Algo, CompressArgs, Compression};
use crate::facts::{Facts, Scope, SortArgs, SortKey};
use crate::pick::{Setting, Value};
use crate::transfer::{Inbox, Outbox};
use crate::units::{self, Act, Candidate, Choice, Ctx, Report, Status, Unit, Verb};
use crate::wire::{self, Launch, Remote, SshArgs, Terms};
use crate::{human_size, Side};

const DELETE_KEY: char = 'D';
const RELOCATE_KEY: char = 'R';

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List the OMP sessions on a remote host.
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
    pub host: String,
    #[command(flatten)]
    pub scope: Scope,
    #[command(flatten)]
    pub sort: SortArgs,
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
    pub host: String,
    #[command(flatten)]
    pub act: Act,
    #[command(flatten)]
    pub scope: Scope,
    #[command(flatten)]
    pub sort: SortArgs,
    /// Remove each session from the host it came from once the other host
    /// has restored it and every checksum passed.
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
        Command::List(args) => list(ctx, &args),
        Command::Pull(args) => migrate(ctx, &args, Direction::Pull),
        Command::Push(args) => migrate(ctx, &args, Direction::Push),
        Command::Serve => wire::serve(ctx),
    }
}

fn only_omp(scope: &Scope) -> Result<()> {
    if !scope.sides().contains(&Side::Pi) {
        bail!("migrate moves OMP sessions only, and the filters leave them out");
    }
    Ok(())
}

fn list(ctx: &Ctx, args: &ListArgs) -> Result<i32> {
    only_omp(&args.scope)?;
    let mut remote = Remote::connect(&args.host, &args.ssh, Launch::default())?;
    let (units, held) = remote.list()?;
    let (host, root) = (remote.host.clone(), PathBuf::from(&remote.pi_root));
    drop(remote);
    let here = Inbox::new(ctx).present().clone();
    let mut shown: Vec<(Facts, &Unit)> = units
        .iter()
        .map(|u| (units::unit_facts(u), u))
        .filter(|(f, _)| args.scope.admits(f))
        .collect();
    let sort = args.sort.resolve(SortKey::Used);
    shown.sort_by(|(a, _), (b, _)| sort.compare(a, b));
    if args.json {
        let rows: Vec<_> = shown
            .iter()
            .map(|(_, u)| json!({"unit": u, "open": held.contains(&u.id), "here": here.contains(&u.id)}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if !args.long {
        units::print_header();
    }
    for (facts, unit) in &shown {
        if args.long {
            println!("── {} ──", if unit.title.is_empty() { &unit.id } else { &unit.title });
            let mut details = units::unit_row(unit, &root, ctx.now_ms).details;
            details.push(("state".into(), state(unit, &held, &here, &host)));
            units::print_details(&details);
            println!();
        } else {
            units::print_line(facts, ctx.now_ms);
        }
    }
    let bytes: u64 = shown.iter().map(|(_, u)| u.bytes).sum();
    let count = |set: &HashSet<String>| shown.iter().filter(|(_, u)| set.contains(&u.id)).count();
    println!(
        "{} OMP session(s) on {} · {} · {} open there · {} already here",
        shown.len(),
        host,
        human_size(bytes),
        count(&held),
        count(&here)
    );
    Ok(0)
}

fn state(unit: &Unit, held: &HashSet<String>, here: &HashSet<String>, host: &str) -> String {
    match (held.contains(&unit.id), here.contains(&unit.id)) {
        (true, _) => format!("open in a running omp on {host}"),
        (false, true) => "already here".into(),
        (false, false) => "can migrate".into(),
    }
}

/// The sessions on the sending host, and what blocks them.
pub struct Plan {
    pub sources: Vec<Unit>,
    pub root: PathBuf,
    held: HashSet<String>,
    /// Ids the receiving host already has.
    present: HashSet<String>,
    pub from: String,
    pub to: String,
}

pub fn plan(ctx: &Ctx, remote: &mut Remote, outbox: &mut Outbox, inbox: &mut Inbox, direction: Direction) -> Result<Plan> {
    let (remote_units, remote_held) = remote.list()?;
    let host = remote.host.clone();
    Ok(match direction {
        Direction::Pull => Plan {
            sources: remote_units,
            root: PathBuf::from(&remote.pi_root),
            held: remote_held,
            present: inbox.present().clone(),
            from: host,
            to: "this host".into(),
        },
        Direction::Push => {
            let sources = outbox.units().to_vec();
            let ids: Vec<String> = sources.iter().map(|u| u.id.clone()).collect();
            Plan {
                held: outbox.held(&ids),
                sources,
                root: ctx.roots.pi.clone(),
                present: remote_units.into_iter().map(|u| u.id).collect(),
                from: "this host".into(),
                to: host,
            }
        }
    })
}

impl Plan {
    pub fn blocked(&self, unit: &Unit) -> Option<Status> {
        if self.held.contains(&unit.id) {
            Some(Status::Held)
        } else if self.present.contains(&unit.id) {
            Some(Status::Conflict(format!("already on {}", self.to)))
        } else {
            None
        }
    }

    pub fn candidate(&self, unit: &Unit, now_ms: i64) -> Candidate {
        let mut row = units::unit_row(unit, &self.root, now_ms);
        row.details.insert(0, ("host".into(), self.from.clone()));
        row.blocked = match self.blocked(unit) {
            Some(Status::Held) => Some(format!("open in a running omp on {}", self.from)),
            Some(_) => Some(format!("already on {}", self.to)),
            None => None,
        };
        Candidate { path: None, row }
    }
}

fn settings(args: &MoveArgs) -> Vec<Setting> {
    vec![
        Setting { key: DELETE_KEY, label: "delete source".into(), value: Value::Flag(args.delete), warn: true },
        Setting {
            key: RELOCATE_KEY,
            label: "relocate to".into(),
            value: Value::Text(args.relocate.clone().unwrap_or_default()),
            warn: false,
        },
    ]
}

/// The delete flag and the relocation directory, as the picker left them.
fn chosen_settings(settings: &[Setting]) -> (bool, Option<String>) {
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

fn migrate(ctx: &Ctx, args: &MoveArgs, direction: Direction) -> Result<i32> {
    only_omp(&args.scope)?;
    let compression = args.compress.resolve(Algo::Zstd)?;
    let mut remote = Remote::connect(&args.host, &args.ssh, Launch::default())?;
    let mut outbox = Outbox::new(ctx);
    let mut inbox = Inbox::new(ctx);
    let plan = plan(ctx, &mut remote, &mut outbox, &mut inbox, direction)?;

    let candidates = plan.sources.iter().map(|u| plan.candidate(u, ctx.now_ms)).collect();
    let arrow = match direction {
        Direction::Pull => format!("← {}", remote.host),
        Direction::Push => format!("→ {}", remote.host),
    };
    let choice = Choice {
        title: format!("migrate {arrow} · {compression}"),
        verb: "migrate",
        threshold: None,
        sort: args.sort.resolve(SortKey::Used),
        now_ms: ctx.now_ms,
    };
    let mut settings = settings(args);
    let chosen = units::choose_with(candidates, &args.act, &args.scope, choice, &mut settings)?;
    if chosen.is_empty() {
        println!("nothing chosen");
        return Ok(0);
    }
    let terms = terms(&settings, direction, &mut remote, compression)?;
    println!("{}", describe(&plan, &terms));

    let mut report = Report::new(Verb::Migrate, args.act.dry_run);
    for i in chosen {
        let unit = &plan.sources[i];
        let mut sizes = human_size(unit.bytes);
        let status = if let Some(status) = plan.blocked(unit) {
            status
        } else if args.act.dry_run {
            Status::Would
        } else {
            let (status, archive) = match direction {
                Direction::Pull => remote.fetch(&unit.id, &terms, &mut inbox)?,
                Direction::Push => remote.deliver(&unit.id, &terms, &mut outbox)?,
            };
            if archive > 0 {
                sizes = format!("{} → {}", human_size(unit.bytes), human_size(archive));
            }
            status
        };
        if status == Status::Done || status == Status::Would {
            match direction {
                Direction::Pull => report.restored += unit.bytes,
                Direction::Push if terms.delete => report.freed += unit.bytes,
                Direction::Push => {}
            }
        }
        report.line(&status, Side::Pi, &unit.id, &unit.title, &sizes);
    }
    Ok(report.finish())
}

/// The terms the picker or the flags left, with the relocation target
/// resolved on the receiving host.
fn terms(settings: &[Setting], direction: Direction, remote: &mut Remote, compression: Compression) -> Result<Terms> {
    let (delete, relocate) = chosen_settings(settings);
    let relocate = relocate.map(|dir| target(&dir, direction, remote)).transpose()?;
    Ok(Terms { compression, relocate, delete })
}

/// A relocation directory resolved on the receiving host.
pub fn target(dir: &str, direction: Direction, remote: &mut Remote) -> Result<String> {
    match direction {
        Direction::Pull => Inbox::target(dir),
        Direction::Push => remote.target(dir),
    }
}

/// Migrates one session; an `Err` broke the connection.
pub fn one(remote: &mut Remote, direction: Direction, id: &str, terms: &Terms, inbox: &mut Inbox, outbox: &mut Outbox) -> Result<(Status, u64)> {
    match direction {
        Direction::Pull => remote.fetch(id, terms, inbox),
        Direction::Push => remote.deliver(id, terms, outbox),
    }
}

fn describe(plan: &Plan, terms: &Terms) -> String {
    let mut line = format!("{} → {} · {}", plan.from, plan.to, terms.compression);
    if let Some(dir) = &terms.relocate {
        line.push_str(&format!(" · relocate to {dir}"));
    }
    if terms.delete {
        line.push_str(" · delete source");
    }
    line
}
