//! Sessions as on-disk units — every path one session owns — with the age and
//! size an archiving decision needs, and the choosing, rows and reporting the
//! `stale`, `compact` and `vault` commands share.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, IsTerminal};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::archive::{Manifest, Refusal, System};
use crate::facts::{self, Facts, Scope, Sort, SortArgs, SortKey};
use crate::paths::{self, Roots};
use crate::{claude, clip, convert, dsh, human_size, pick, relocate, Side};

const DAY_MS: f64 = 86_400_000.0;

/// What the archiving commands run against.
pub struct Ctx {
    pub roots: Roots,
    /// `omp`, to ask which Pi sessions a running omp holds.
    pub omp: Option<PathBuf>,
    pub system: System,
    pub now_ms: i64,
}

impl Ctx {
    pub fn from_env() -> Ctx {
        Ctx {
            roots: Roots::from_env(),
            omp: relocate::which("omp"),
            system: System::current(),
            now_ms: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// One session as the paths it owns.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Unit {
    pub harness: Side,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub bucket: String,
    /// What the members are named after: the transcript stem (Pi), the
    /// session directory (DSH) or the session id (Claude Code).
    pub name: String,
    /// Paths the session owns, relative to its harness root: `<bucket>/<entry>`.
    pub members: Vec<String>,
    pub created_ms: i64,
    /// Newest modification time of anything the session owns.
    pub last_used_ms: i64,
    pub bytes: u64,
    pub files: usize,
    pub subagents: usize,
    /// Conversation messages, when discovery counted them.
    pub messages: Option<usize>,
}

/// What a path under a session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// Symlink, socket, device: nothing an archive carries.
    Other,
}

/// One path under a session, as found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Path relative to the harness root, `/`-separated.
    pub rel: String,
    pub kind: Kind,
    pub size: u64,
    pub mode: u32,
    pub mtime: SystemTime,
}

/// Every path under `members`, parents before children, in name order.
pub fn walk(root: &Path, members: &[String]) -> Result<Vec<Node>> {
    let mut out = Vec::new();
    for member in members {
        let top = root.join(member);
        for entry in WalkDir::new(&top).follow_links(false).sort_by_file_name() {
            let entry = entry.with_context(|| format!("walk {}", top.display()))?;
            let meta = entry.metadata().with_context(|| format!("stat {}", entry.path().display()))?;
            let kind = if meta.is_file() {
                Kind::File
            } else if meta.is_dir() {
                Kind::Dir
            } else {
                Kind::Other
            };
            out.push(Node {
                rel: relative(root, entry.path())?,
                kind,
                size: if kind == Kind::File { meta.len() } else { 0 },
                mode: mode(&meta),
                mtime: meta.modified().unwrap_or(UNIX_EPOCH),
            });
        }
    }
    Ok(out)
}

/// `path` under `root` as a `/`-joined string.
fn relative(root: &Path, path: &Path) -> Result<String> {
    let rest = path.strip_prefix(root)?;
    let mut parts = Vec::new();
    for part in rest.components() {
        match part {
            Component::Normal(name) => match name.to_str() {
                Some(name) => parts.push(name),
                None => bail!("{} is not valid UTF-8", path.display()),
            },
            _ => bail!("unexpected path {}", path.display()),
        }
    }
    Ok(parts.join("/"))
}

#[cfg(unix)]
pub(crate) fn mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
pub(crate) fn mode(meta: &fs::Metadata) -> u32 {
    if meta.is_dir() {
        0o755
    } else {
        0o644
    }
}

pub fn epoch_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Newest modification time among `nodes`.
pub fn last_used_ms(nodes: &[Node]) -> i64 {
    nodes.iter().map(|n| epoch_ms(n.mtime)).max().unwrap_or(0)
}

/// Every session of the given harnesses.
pub fn discover(roots: &Roots, sides: &[Side]) -> Vec<Unit> {
    let mut out = Vec::new();
    for &side in sides {
        let root = roots.of(side);
        let found = match side {
            Side::Pi => pi_units(root),
            Side::Dsh => dsh_units(root),
            Side::Claude => claude_units(root),
        };
        out.extend(found.into_iter().filter_map(|unit| measured(root, unit)));
    }
    out
}

/// Fills a unit's size, file count and last use from what is on disk.
fn measured(root: &Path, mut unit: Unit) -> Option<Unit> {
    let nodes = walk(root, &unit.members).ok()?;
    let files: Vec<&Node> = nodes.iter().filter(|n| n.kind == Kind::File).collect();
    unit.bytes = files.iter().map(|n| n.size).sum();
    unit.files = files.len();
    unit.last_used_ms = last_used_ms(&nodes);
    if unit.harness == Side::Pi {
        // Subagent transcripts live in the artifact directory.
        unit.subagents = files.iter().filter(|n| n.rel.ends_with(".jsonl")).count().saturating_sub(1);
    }
    Some(unit)
}

/// Every directory directly under `root`, with its name.
fn buckets(root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path())))
        .filter(|(name, _)| !name.starts_with('.'))
        .collect();
    out.sort();
    out
}

/// Top-level Pi transcripts, read only as far as their header.
fn pi_units(root: &Path) -> Vec<Unit> {
    let mut out = Vec::new();
    for (bucket, dir) in buckets(root) {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { continue };
            let Some(stem) = file_name.strip_suffix(".jsonl") else { continue };
            if file_name.starts_with('.') || !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let Some(head) = pi_head(&path) else { continue };
            let mut members = vec![format!("{bucket}/{file_name}")];
            if dir.join(stem).is_dir() {
                members.push(format!("{bucket}/{stem}"));
            }
            let lock = format!(".{file_name}.lock.os");
            if dir.join(&lock).symlink_metadata().is_ok() {
                members.push(format!("{bucket}/{lock}"));
            }
            out.push(Unit {
                harness: Side::Pi,
                id: head.id,
                title: head.title,
                cwd: head.cwd,
                bucket: bucket.clone(),
                name: stem.to_string(),
                members,
                created_ms: convert::from_iso(&head.timestamp).unwrap_or(0),
                last_used_ms: 0,
                bytes: 0,
                files: 0,
                subagents: 0,
                messages: None,
            });
        }
    }
    out
}

struct PiHead {
    id: String,
    title: String,
    cwd: String,
    timestamp: String,
}

/// Title slot and session header of a Pi transcript, or `None` when it is not one.
fn pi_head(path: &Path) -> Option<PiHead> {
    let reader = BufReader::new(File::open(path).ok()?);
    let mut title = String::new();
    for line in reader.lines().take(4) {
        let line = line.ok()?;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line.trim()).ok()?;
        let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        match value.get("type").and_then(Value::as_str) {
            Some("title") => title = text("title"),
            Some("session") => {
                return Some(PiHead {
                    id: text("id"),
                    title: if title.is_empty() { text("title") } else { title },
                    cwd: text("cwd"),
                    timestamp: text("timestamp"),
                })
            }
            _ => return None,
        }
    }
    None
}

fn dsh_units(root: &Path) -> Vec<Unit> {
    dsh::discover(root)
        .into_iter()
        .filter_map(|found| {
            let name = found.path.parent()?.file_name()?.to_str()?.to_string();
            Some(Unit {
                harness: Side::Dsh,
                id: found.id,
                title: found.title.unwrap_or_default(),
                cwd: found.cwd,
                members: vec![format!("{}/{name}", found.bucket)],
                bucket: found.bucket,
                name,
                created_ms: found.created_at,
                last_used_ms: 0,
                bytes: 0,
                files: 0,
                subagents: 0,
                messages: Some(found.messages),
            })
        })
        .collect()
}

fn claude_units(root: &Path) -> Vec<Unit> {
    claude::discover(root)
        .into_iter()
        .filter_map(|found| {
            let file_name = found.path.file_name()?.to_str()?.to_string();
            let name = file_name.strip_suffix(".jsonl")?.to_string();
            let mut members = vec![format!("{}/{file_name}", found.bucket)];
            if found.path.with_extension("").is_dir() {
                members.push(format!("{}/{name}", found.bucket));
            }
            Some(Unit {
                harness: Side::Claude,
                title: found.label().to_string(),
                id: found.id,
                cwd: found.cwd,
                bucket: found.bucket,
                name,
                members,
                created_ms: found.started,
                last_used_ms: 0,
                bytes: 0,
                files: 0,
                subagents: found.subagents,
                messages: Some(found.messages),
            })
        })
        .collect()
}

/// A span of time: `30d`, `2w`, `36h`, or a bare number of days.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Age {
    pub days: f64,
}

impl FromStr for Age {
    type Err = String;

    fn from_str(text: &str) -> Result<Age, String> {
        let text = text.trim();
        let split = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
        let (number, unit) = text.split_at(split);
        let value: f64 = number.parse().map_err(|_| format!("{text:?} is not an age such as 30d, 2w or 36h"))?;
        let days = match unit {
            "" | "d" => value,
            "w" => value * 7.0,
            "h" => value / 24.0,
            _ => return Err(format!("{text:?}: use d (days), w (weeks) or h (hours)")),
        };
        Ok(Age { days })
    }
}

impl fmt::Display for Age {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.days.fract() == 0.0 {
            write!(f, "{}d", self.days)
        } else {
            write!(f, "{}h", (self.days * 24.0).round())
        }
    }
}

pub fn age_days(now_ms: i64, then_ms: i64) -> f64 {
    (now_ms - then_ms).max(0) as f64 / DAY_MS
}

/// Compact age for a list column.
pub fn age_label(days: f64) -> String {
    if days < 1.0 {
        format!("{}h", (days * 24.0).floor())
    } else {
        format!("{}d", days.floor())
    }
}

/// How a command chooses what to act on.
#[derive(Debug, Args)]
pub struct Act {
    /// Session ids, id prefixes, or archive paths. Without any, a picker opens
    /// on a terminal.
    pub sessions: Vec<String>,
    /// Act on every match without the picker.
    #[arg(short, long)]
    pub yes: bool,
    /// Print what would happen and change nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// One thing the user can choose.
pub struct Candidate {
    /// Archive file, for candidates the user can name by path.
    pub path: Option<PathBuf>,
    pub row: pick::Row,
}

/// The picker's framing and the order of one command.
pub struct Choice {
    pub title: String,
    pub verb: &'static str,
    pub threshold: Option<Age>,
    pub sort: Sort,
    pub now_ms: i64,
}

/// Columns of every picker.
pub const HEADERS: [&str; 6] = ["unused", "size", "archive", "harness", "project", "title"];

/// Picks which candidates to act on, as indices into `candidates`: the named
/// ones; else, under `--yes` (or `--dry-run` off a terminal), every match old
/// enough; else the user's picks. Cancelling picks nothing.
pub fn choose(candidates: Vec<Candidate>, act: &Act, scope: &Scope, choice: Choice) -> Result<Vec<usize>> {
    choose_with(candidates, act, scope, choice, &mut Vec::new())
}

/// [`choose`], letting the picker change `settings`; they come back as the user left them.
pub fn choose_with(
    candidates: Vec<Candidate>,
    act: &Act,
    scope: &Scope,
    choice: Choice,
    settings: &mut Vec<pick::Setting>,
) -> Result<Vec<usize>> {
    let mut pool: Vec<(usize, Candidate)> =
        candidates.into_iter().enumerate().filter(|(_, c)| scope.admits(&c.row.facts)).collect();
    pool.sort_by(|(_, a), (_, b)| choice.sort.compare(&a.row.facts, &b.row.facts));
    if !act.sessions.is_empty() {
        return named(&pool, &act.sessions);
    }
    let old_enough = |c: &Candidate| {
        choice.threshold.is_none_or(|age| age_days(choice.now_ms, c.row.facts.last_used_ms) >= age.days)
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if act.yes || (act.dry_run && !interactive) {
        return Ok(pool
            .iter()
            .filter(|(_, c)| c.row.blocked.is_none() && old_enough(c))
            .map(|(i, _)| *i)
            .collect());
    }
    if !interactive {
        bail!("no terminal for the picker: name the sessions, pass --yes, or preview with --dry-run");
    }
    let (index, rows): (Vec<usize>, Vec<pick::Row>) = pool.into_iter().map(|(i, c)| (i, c.row)).unzip();
    let spec = pick::Spec {
        title: choice.title,
        verb: choice.verb.to_string(),
        headers: HEADERS.iter().map(|h| h.to_string()).collect(),
        rows,
        threshold_days: choice.threshold.map(|age| age.days.round() as u32),
        sort: choice.sort,
        now_ms: choice.now_ms,
        settings: settings.clone(),
        extra_keys: Vec::new(),
    };
    let Some(picked) = pick::pick(spec)? else {
        return Ok(Vec::new());
    };
    *settings = picked.settings;
    Ok(picked.rows.into_iter().map(|k| index[k]).collect())
}

/// Candidates the user named: an archive path, or an id prefix that names one
/// session. Several candidates of that session resolve to the one offered
/// first, whatever order the listing is sorted in.
fn named(pool: &[(usize, Candidate)], names: &[String]) -> Result<Vec<usize>> {
    let mut out: Vec<usize> = Vec::new();
    for name in names {
        let wanted = fs::canonicalize(name).ok();
        if let Some((i, _)) = pool
            .iter()
            .find(|(_, c)| wanted.is_some() && c.path.as_ref().and_then(|p| fs::canonicalize(p).ok()) == wanted)
        {
            out.push(*i);
            continue;
        }
        let lower = name.to_lowercase();
        let hits: Vec<&(usize, Candidate)> =
            pool.iter().filter(|(_, c)| c.row.facts.id.to_lowercase().starts_with(&lower)).collect();
        let ids: BTreeSet<&str> = hits.iter().map(|(_, c)| c.row.facts.id.as_str()).collect();
        match ids.len() {
            0 => bail!("nothing here matches {name:?}"),
            1 => out.extend(hits.iter().map(|(i, _)| *i).min()),
            _ => bail!("{name:?} matches several sessions: {}", ids.into_iter().collect::<Vec<_>>().join(", ")),
        }
    }
    out.dedup();
    Ok(out)
}

/// Pi sessions a running omp holds. Without omp nothing counts as held, and
/// packing relies on the session's files staying unchanged.
pub fn held_pi(ctx: &Ctx, units: &[&Unit]) -> HashSet<String> {
    let ids: Vec<String> = units.iter().filter(|u| u.harness == Side::Pi).map(|u| u.id.clone()).collect();
    if ids.is_empty() {
        return HashSet::new();
    }
    let Some(omp) = ctx.omp.as_deref() else {
        eprintln!("note: omp is not on PATH; open Pi sessions cannot be detected");
        return HashSet::new();
    };
    relocate::lease(omp, &ids).unwrap_or_else(|err| {
        eprintln!("note: cannot ask omp which sessions are open ({err:#})");
        HashSet::new()
    })
}

/// Removes every member of a session from `root`.
pub fn remove_members(root: &Path, members: &[String]) -> Result<()> {
    for member in members {
        let path = root.join(member);
        let result = match path.symlink_metadata() {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(_) => continue,
        };
        result.with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

/// A directory with the home prefix shown as `~`.
pub fn tilde(path: &str) -> String {
    let home = paths::home();
    match Path::new(path).strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.to_string(),
    }
}

/// What a live session sorts and filters by.
pub fn unit_facts(unit: &Unit) -> Facts {
    Facts {
        harness: unit.harness,
        id: unit.id.clone(),
        title: unit.title.clone(),
        path: unit.cwd.clone(),
        project: facts::project_name(&unit.cwd),
        created_ms: unit.created_ms,
        last_used_ms: unit.last_used_ms,
        bytes: unit.bytes,
        archive_bytes: None,
    }
}

/// What an archived session sorts and filters by.
pub fn manifest_facts(m: &Manifest, archive_bytes: u64) -> Facts {
    Facts {
        harness: m.harness,
        id: m.id.clone(),
        title: m.title.clone(),
        path: m.cwd.clone(),
        project: facts::project_name(&m.cwd),
        created_ms: convert::from_iso(&m.created).unwrap_or(0),
        last_used_ms: m.last_used_ms(),
        bytes: m.bytes,
        archive_bytes: Some(archive_bytes),
    }
}

/// Picker cells, in [`HEADERS`] order.
fn columns(f: &Facts, now_ms: i64) -> Vec<String> {
    vec![
        age_label(age_days(now_ms, f.last_used_ms)),
        human_size(f.bytes),
        f.archive_bytes.map_or_else(|| "—".to_string(), human_size),
        f.harness.key().to_string(),
        f.project.clone(),
        title_or_id(&f.title, &f.id),
    ]
}

/// Picker row for a live session under `root`.
pub fn unit_row(unit: &Unit, root: &Path, now_ms: i64) -> pick::Row {
    let facts = unit_facts(unit);
    pick::Row { columns: columns(&facts, now_ms), facts, details: unit_details(unit, root, now_ms), blocked: None }
}

/// Picker row for an archived session.
pub fn archive_row(manifest: &Manifest, archive_bytes: u64, path: &Path, now_ms: i64) -> pick::Row {
    let facts = manifest_facts(manifest, archive_bytes);
    pick::Row {
        columns: columns(&facts, now_ms),
        facts,
        details: manifest_details(manifest, archive_bytes, path, now_ms),
        blocked: None,
    }
}

fn title_or_id(title: &str, id: &str) -> String {
    if title.is_empty() {
        format!("({id})")
    } else {
        title.to_string()
    }
}

/// An ISO time with how long ago it was.
pub fn when(iso: &str, now_ms: i64) -> String {
    match convert::from_iso(iso) {
        Some(ms) => format!("{iso} ({} ago)", age_label(age_days(now_ms, ms))),
        None => iso.to_string(),
    }
}

fn detail(label: &str, value: impl Into<String>) -> (String, String) {
    (label.to_string(), value.into())
}

/// Everything known about a live session, for the picker.
fn unit_details(unit: &Unit, root: &Path, now_ms: i64) -> Vec<(String, String)> {
    let mut out = vec![
        detail("id", unit.id.clone()),
        detail("title", unit.title.clone()),
        detail("harness", unit.harness.key()),
        detail("project", facts::project_name(&unit.cwd)),
        detail("path", unit.cwd.clone()),
        detail("created", when(&convert::iso(unit.created_ms), now_ms)),
        detail("last used", when(&convert::iso(unit.last_used_ms), now_ms)),
        detail("size", format!("{} in {} files", human_size(unit.bytes), unit.files)),
        detail("subagents", unit.subagents.to_string()),
    ];
    if let Some(messages) = unit.messages {
        out.push(detail("messages", messages.to_string()));
    }
    out.push(detail("paths", unit.members.iter().map(|m| root.join(m).display().to_string()).collect::<Vec<_>>().join("\n")));
    out
}

/// Everything an archive records about its session.
pub fn manifest_details(m: &Manifest, archive_bytes: u64, path: &Path, now_ms: i64) -> Vec<(String, String)> {
    let ratio = if m.bytes == 0 { 0.0 } else { archive_bytes as f64 * 100.0 / m.bytes as f64 };
    let mut out = vec![
        detail("id", m.id.clone()),
        detail("title", m.title.clone()),
        detail("harness", m.harness.key()),
        detail("project", facts::project_name(&m.cwd)),
        detail("path", m.cwd.clone()),
        detail("created", when(&m.created, now_ms)),
        detail("last used", when(&m.last_used, now_ms)),
        detail("size", format!("{} in {} files", human_size(m.bytes), m.files)),
        detail("subagents", m.subagents.to_string()),
    ];
    if let Some(messages) = m.messages {
        out.push(detail("messages", messages.to_string()));
    }
    out.extend([
        detail("archive", format!("{} ({ratio:.0}% of the original)", human_size(archive_bytes))),
        detail("compression", m.compression.to_string()),
        detail("packed", when(&m.packed_at, now_ms)),
        detail("packed on", m.packed_on.describe()),
        detail("packed from", m.members.iter().map(|p| format!("{}/{p}", m.root)).collect::<Vec<_>>().join("\n")),
        detail("file", path.display().to_string()),
    ]);
    out
}

/// Prints detail pairs as an aligned block.
pub fn print_details(details: &[(String, String)]) {
    let width = details.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    for (label, value) in details {
        let mut lines = value.lines();
        println!("  {label:<width$}  {}", lines.next().unwrap_or_default());
        for more in lines {
            println!("  {:<width$}  {more}", "");
        }
    }
}

/// One listing line: age, sizes, harness, id, project, path, title.
pub fn print_line(f: &Facts, now_ms: i64) {
    println!(
        "{:>6}  {:>9}  {:>9}  {:<6}  {:<36}  {:<20}  {:<32}  {}",
        age_label(age_days(now_ms, f.last_used_ms)),
        human_size(f.bytes),
        f.archive_bytes.map_or_else(|| "—".to_string(), human_size),
        f.harness.key(),
        f.id,
        clip(&f.project, 20),
        crate::shorten(&tilde(&f.path), 32),
        clip(&f.title, 60)
    );
}

pub fn print_header() {
    println!(
        "{:>6}  {:>9}  {:>9}  {:<6}  {:<36}  {:<20}  {:<32}  title",
        "unused", "size", "archive", "harness", "id", "project", "path"
    );
}

/// The archiving actions, for report wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Pack,
    Unpack,
    Push,
    Pull,
    Offload,
    Migrate,
}

impl Verb {
    fn present(self) -> &'static str {
        match self {
            Verb::Pack => "pack",
            Verb::Unpack => "unpack",
            Verb::Push => "push",
            Verb::Pull => "pull",
            Verb::Offload => "offload",
            Verb::Migrate => "migrate",
        }
    }

    fn past(self) -> &'static str {
        match self {
            Verb::Pack => "packed",
            Verb::Unpack => "unpacked",
            Verb::Push => "pushed",
            Verb::Pull => "pulled",
            Verb::Offload => "offloaded",
            Verb::Migrate => "migrated",
        }
    }
}

/// What happened to one chosen session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Done,
    Would,
    /// A running omp holds it.
    Held,
    /// The store already holds identical content.
    Stored,
    Conflict(String),
    /// It changed while being packed.
    Changed,
    Failed(String),
}

impl Status {
    /// Maps an error to the refusal it carries, else a failure.
    pub fn from_error(err: anyhow::Error) -> Status {
        match err.downcast_ref::<Refusal>() {
            Some(Refusal::Conflict(what)) => Status::Conflict(what.clone()),
            Some(Refusal::Changed) => Status::Changed,
            None => Status::Failed(format!("{err:#}")),
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self, Status::Done | Status::Would | Status::Stored)
    }

    pub(crate) fn label(&self, verb: Verb) -> String {
        match self {
            Status::Done => verb.past().to_string(),
            Status::Would => format!("would-{}", verb.present()),
            Status::Held => "held".into(),
            Status::Stored => "in-store".into(),
            Status::Conflict(_) => "conflict".into(),
            Status::Changed => "changed".into(),
            Status::Failed(_) => "error".into(),
        }
    }

    pub(crate) fn reason(&self) -> Option<&str> {
        match self {
            Status::Conflict(why) | Status::Failed(why) => Some(why),
            Status::Held => Some("open in a running omp"),
            Status::Changed => Some("written to while packing; retry once it is idle"),
            Status::Stored => Some("the store already holds this exact session"),
            _ => None,
        }
    }
}

/// Tally of one run, printed line by line.
pub struct Report {
    verb: Verb,
    dry_run: bool,
    done: usize,
    skipped: usize,
    /// Local bytes released.
    pub freed: u64,
    /// Local bytes restored.
    pub restored: u64,
}

impl Report {
    pub fn new(verb: Verb, dry_run: bool) -> Report {
        Report { verb, dry_run, done: 0, skipped: 0, freed: 0, restored: 0 }
    }

    pub fn line(&mut self, status: &Status, harness: Side, id: &str, title: &str, sizes: &str) {
        if status.ok() {
            self.done += 1;
        } else {
            self.skipped += 1;
        }
        let reason = status.reason().map(|r| format!("  ({r})")).unwrap_or_default();
        println!(
            "{:<12} {:<6} {:<36}  {:>21}  {}{reason}",
            status.label(self.verb),
            harness.key(),
            id,
            sizes,
            clip(title, 48)
        );
    }

    /// Prints the summary; the exit status is 1 when anything was skipped.
    pub fn finish(&self) -> i32 {
        let action = if self.dry_run { format!("would {}", self.verb.present()) } else { self.verb.past().to_string() };
        let mut summary = format!("{} {action} · {} skipped", self.done, self.skipped);
        if self.freed > 0 {
            summary.push_str(&format!(" · {} {}", human_size(self.freed), if self.dry_run { "to free" } else { "freed" }));
        }
        if self.restored > 0 {
            summary.push_str(&format!(" · {} restored", human_size(self.restored)));
        }
        println!("{summary}");
        i32::from(self.skipped > 0)
    }
}

/// `pi-transfuse stale`: live sessions unused for a while.
#[derive(Debug, Args)]
pub struct StaleArgs {
    /// Unused for at least this long: 30d, 2w, 36h, or days.
    #[arg(long, default_value = "30d")]
    pub older_than: Age,
    #[command(flatten)]
    pub scope: Scope,
    #[command(flatten)]
    pub sort: SortArgs,
    /// Print JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

pub fn stale(ctx: &Ctx, args: &StaleArgs) -> Result<()> {
    let all = discover(&ctx.roots, &args.scope.sides());
    let total_bytes: u64 = all.iter().map(|u| u.bytes).sum();
    let total = all.len();
    let mut old: Vec<(Facts, Unit)> = all
        .into_iter()
        .filter(|u| age_days(ctx.now_ms, u.last_used_ms) >= args.older_than.days)
        .map(|u| (unit_facts(&u), u))
        .filter(|(f, _)| args.scope.admits(f))
        .collect();
    let sort = args.sort.resolve(SortKey::Size);
    old.sort_by(|(a, _), (b, _)| sort.compare(a, b));
    if args.json {
        let rows: Vec<Value> = old.iter().map(|(_, u)| unit_json(u, ctx)).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    print_header();
    for (facts, _) in &old {
        print_line(facts, ctx.now_ms);
    }
    let mut by_path: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    for (_, unit) in &old {
        let slot = by_path.entry(unit.cwd.as_str()).or_default();
        slot.0 += 1;
        slot.1 += unit.bytes;
    }
    let mut by_path: Vec<(&str, (usize, u64))> = by_path.into_iter().collect();
    by_path.sort_by_key(|(_, (_, bytes))| std::cmp::Reverse(*bytes));
    if !by_path.is_empty() {
        println!("\nby path:");
        for (cwd, (count, bytes)) in by_path {
            println!("  {:>9}  {count:>4} session(s)  {}", human_size(bytes), tilde(cwd));
        }
    }
    let bytes: u64 = old.iter().map(|(_, u)| u.bytes).sum();
    println!(
        "\n{} session(s) unused ≥ {} · {} (of {} across {total} sessions)",
        old.len(),
        args.older_than,
        human_size(bytes),
        human_size(total_bytes)
    );
    Ok(())
}

fn unit_json(unit: &Unit, ctx: &Ctx) -> Value {
    let root = ctx.roots.of(unit.harness);
    json!({
        "harness": unit.harness,
        "project": facts::project_name(&unit.cwd),
        "id": unit.id,
        "title": unit.title,
        "cwd": unit.cwd,
        "bucket": unit.bucket,
        "members": unit.members.iter().map(|m| root.join(m)).collect::<Vec<_>>(),
        "created": convert::iso(unit.created_ms),
        "lastUsed": convert::iso(unit.last_used_ms),
        "unusedDays": (age_days(ctx.now_ms, unit.last_used_ms) * 10.0).round() / 10.0,
        "bytes": unit.bytes,
        "files": unit.files,
        "subagents": unit.subagents,
        "messages": unit.messages,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// Scratch harness roots, removed on drop.
    pub struct Scratch {
        pub dir: PathBuf,
        pub roots: Roots,
    }

    impl Scratch {
        pub fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "pi-transfuse-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&dir);
            let roots = Roots { dsh: dir.join("dsh"), pi: dir.join("pi"), claude: dir.join("claude") };
            for root in [&roots.dsh, &roots.pi, &roots.claude] {
                fs::create_dir_all(root).unwrap();
            }
            Scratch { dir, roots }
        }

        pub fn ctx(&self) -> Ctx {
            Ctx { roots: self.roots.clone(), omp: None, system: System::current(), now_ms: 2_000_000_000_000 }
        }

        /// A Pi session with an artifact directory holding a subagent, an
        /// empty directory and a read-only file, and a lock sidecar.
        pub fn pi_session(&self, id: &str, cwd: &str) -> PathBuf {
            let bucket = self.roots.pi.join(paths::encode_pi_bucket(cwd));
            let stem = format!("2026-01-01T00-00-00-000Z_{id}");
            let transcript = bucket.join(format!("{stem}.jsonl"));
            fs::create_dir_all(bucket.join(&stem).join("deep/empty")).unwrap();
            fs::write(
                &transcript,
                format!(
                    "{}\n{}\n{}\n",
                    crate::pi::title_slot(&format!("title {id}"), None, "2026-01-01T00:00:00.000Z"),
                    json!({"type":"session","version":3,"id":id,"timestamp":"2026-01-01T00:00:00.000Z","cwd":cwd}),
                    r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}"#,
                ),
            )
            .unwrap();
            fs::write(bucket.join(&stem).join("deep/agent.jsonl"), "{\"type\":\"session\"}\n").unwrap();
            let locked = bucket.join(&stem).join("notes.txt");
            fs::write(&locked, vec![7u8; 70_000]).unwrap();
            set_readonly(&locked);
            fs::write(bucket.join(format!(".{stem}.jsonl.lock.os")), "").unwrap();
            transcript
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            for entry in WalkDir::new(&self.dir).into_iter().filter_map(Result::ok) {
                make_writable(entry.path());
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    fn set_readonly(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o400)).unwrap();
    }

    #[cfg(unix)]
    fn make_writable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::symlink_metadata(path) {
            let mode = meta.permissions().mode() | 0o700;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
        }
    }

    /// Bytes, modes and second-precision mtimes of every path under `root`.
    /// Bucket directories carry no time: sessions leaving and returning move it.
    pub fn tree(root: &Path) -> BTreeMap<String, (Vec<u8>, u32, u64)> {
        WalkDir::new(root)
            .min_depth(1)
            .into_iter()
            .filter_map(Result::ok)
            .map(|e| {
                let meta = e.metadata().unwrap();
                let bytes = if meta.is_file() { fs::read(e.path()).unwrap() } else { Vec::new() };
                let mtime = if e.depth() == 1 {
                    0
                } else {
                    meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs()
                };
                (relative(root, e.path()).unwrap(), (bytes, mode(&meta), mtime))
            })
            .collect()
    }

    #[test]
    fn ages_parse_in_days_weeks_and_hours() {
        assert_eq!("30d".parse::<Age>().unwrap().days, 30.0);
        assert_eq!("2w".parse::<Age>().unwrap().days, 14.0);
        assert_eq!("36h".parse::<Age>().unwrap().days, 1.5);
        assert_eq!("45".parse::<Age>().unwrap().days, 45.0);
        assert!("30m".parse::<Age>().is_err());
        assert!("soon".parse::<Age>().is_err());
    }

    #[test]
    fn a_pi_unit_owns_transcript_artifacts_and_lock() {
        let scratch = Scratch::new("units");
        scratch.pi_session("aaa", "/work/proj");
        let units = discover(&scratch.roots, &[Side::Pi]);
        assert_eq!(units.len(), 1);
        let unit = &units[0];
        let bucket = paths::encode_pi_bucket("/work/proj");
        assert_eq!(unit.id, "aaa");
        assert_eq!(unit.title, "title aaa");
        assert_eq!(
            unit.members,
            vec![
                format!("{bucket}/2026-01-01T00-00-00-000Z_aaa.jsonl"),
                format!("{bucket}/2026-01-01T00-00-00-000Z_aaa"),
                format!("{bucket}/.2026-01-01T00-00-00-000Z_aaa.jsonl.lock.os"),
            ]
        );
        assert_eq!(unit.files, 4);
        assert_eq!(unit.subagents, 1);
        assert!(unit.bytes > 70_000);
    }

    #[test]
    fn dsh_and_claude_units_own_their_directories() {
        let scratch = Scratch::new("units-other");
        let dsh_dir = scratch.roots.dsh.join("--work-proj--").join("d1");
        fs::create_dir_all(&dsh_dir).unwrap();
        let header = json!({"type":"session","version":4,"id":"d1","createdAt":1,"cwd":"/work/proj"});
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        std::io::Write::write_all(&mut encoder, format!("{header}\n").as_bytes()).unwrap();
        fs::write(dsh_dir.join("session.v4.jsonl.zstd"), encoder.finish().unwrap()).unwrap();
        fs::write(dsh_dir.join("session.lock"), "").unwrap();

        let claude_bucket = scratch.roots.claude.join("-work-proj");
        fs::create_dir_all(claude_bucket.join("c1/subagents")).unwrap();
        fs::write(
            claude_bucket.join("c1.jsonl"),
            "{\"type\":\"user\",\"cwd\":\"/work/proj\",\"timestamp\":\"2026-01-01T00:00:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();
        fs::write(claude_bucket.join("c1/subagents/agent-x.jsonl"), "{}\n").unwrap();

        let units = discover(&scratch.roots, &[Side::Dsh, Side::Claude]);
        let dsh = units.iter().find(|u| u.harness == Side::Dsh).unwrap();
        assert_eq!(dsh.members, vec!["--work-proj--/d1".to_string()]);
        assert_eq!(dsh.files, 2);
        let claude = units.iter().find(|u| u.harness == Side::Claude).unwrap();
        assert_eq!(claude.members, vec!["-work-proj/c1.jsonl".to_string(), "-work-proj/c1".to_string()]);
        assert_eq!(claude.files, 2);
    }
}
