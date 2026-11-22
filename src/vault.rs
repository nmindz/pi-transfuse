//! Backup stores: any folder becomes a self-describing store that sessions
//! move away to and come back from. Each archive carries its own manifest; the
//! store index caches them with push and restore history, and `vault reindex`
//! rebuilds it from the archives alone.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use chrono::TimeZone;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::archive::{self, Archive, CompressArgs, Manifest, System};
use crate::compact::{self, Local};
use crate::units::{self, Act, Age, Candidate, Choice, Ctx, Report, Scope, Status, Unit, Verb};
use crate::{convert, human_size, relocate, Side};

/// The store index, at the top of the store folder.
pub const INDEX: &str = "pi-transfuse-store.json";
/// The index as it was before its last write, for `vault reindex` to recover
/// the store's identity and history from.
const PREVIOUS: &str = ".pi-transfuse-store.prev.json";
const LOCK: &str = ".pi-transfuse-store.lock";
/// Sessions pushed between index writes; a synced drive uploads every write.
const SAVE_EVERY: usize = 25;
const REINDEX_HINT: &str = "the store index could not be written; `pi-transfuse vault reindex` rebuilds it from the archives";
const ARCHIVES: &str = "archives";
const FORMAT: &str = "pi-transfuse-store";
const VERSION: u32 = 1;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Make an existing folder a backup store.
    Init {
        /// Folder to hold the store: an external disk, a synced drive, anything mounted.
        store: PathBuf,
        /// Name shown for the store; the folder name by default.
        #[arg(long)]
        name: Option<String>,
    },
    /// Show a store's identity and what it holds by harness, origin and project.
    Info {
        /// Store folder.
        store: PathBuf,
    },
    /// List the sessions a store holds, with what their archives record.
    List(ListArgs),
    /// Move sessions unused for a while into a store; sessions packed in place
    /// move as they are.
    Push(PushArgs),
    /// Restore sessions from a store to the paths they were packed from.
    Pull(PullArgs),
    /// Verify every archive in a store and rebuild its index from them.
    Reindex {
        /// Store folder.
        store: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Store folder.
    pub store: PathBuf,
    #[command(flatten)]
    pub scope: Scope,
    /// Print every recorded detail of each session.
    #[arg(short, long)]
    pub long: bool,
    /// Print JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct PushArgs {
    /// Store folder.
    pub store: PathBuf,
    #[command(flatten)]
    pub act: Act,
    #[command(flatten)]
    pub scope: Scope,
    /// Offer sessions unused for at least this long: 30d, 2w, 36h, or days.
    #[arg(long, default_value = "30d")]
    pub older_than: Age,
    #[command(flatten)]
    pub compress: CompressArgs,
    /// Copy into the store and leave the sessions where they are.
    #[arg(long)]
    pub keep: bool,
}

#[derive(Debug, Args)]
pub struct PullArgs {
    /// Store folder.
    pub store: PathBuf,
    #[command(flatten)]
    pub act: Act,
    #[command(flatten)]
    pub scope: Scope,
    /// Delete the store's copy once the session is restored.
    #[arg(long)]
    pub remove: bool,
}

pub fn run(ctx: &Ctx, command: Command) -> Result<i32> {
    match command {
        Command::Init { store, name } => init(ctx, &store, name),
        Command::Info { store } => info(ctx, &store),
        Command::List(args) => list(ctx, &args),
        Command::Push(args) => push(ctx, &args),
        Command::Pull(args) => pull(ctx, &args),
        Command::Reindex { store } => reindex(ctx, &store),
    }
}

/// A store's identity and contents.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Index {
    pub format: String,
    pub version: u32,
    pub id: String,
    pub name: String,
    pub created: String,
    pub created_on: System,
    pub updated: String,
    pub updated_on: System,
    pub archives: Vec<Entry>,
}

/// One archive the store holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Archive file, relative to the store.
    pub file: String,
    pub archive_bytes: u64,
    pub archive_sha256: String,
    pub content_sha256: String,
    pub pushed: Place,
    pub manifest: Manifest,
    #[serde(default)]
    pub restores: Vec<Place>,
}

/// When, from or to which harness root, and on which machine a session moved.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Place {
    pub at: String,
    pub root: String,
    #[serde(flatten)]
    pub system: System,
}

impl Place {
    fn here(ctx: &Ctx, root: &Path) -> Place {
        Place { at: convert::iso(ctx.now_ms), root: root.display().to_string(), system: ctx.system.clone() }
    }
}

impl Entry {
    fn new(file: String, archive: &Archive, pushed: Place) -> Entry {
        Entry {
            file,
            archive_bytes: archive.archive_bytes,
            archive_sha256: archive.archive_sha256.clone(),
            content_sha256: archive.content_sha256.clone(),
            pushed,
            manifest: archive.manifest.clone(),
            restores: Vec::new(),
        }
    }

    fn search(&self) -> String {
        format!("{} {} {}", self.manifest.id, self.manifest.title, self.manifest.cwd)
    }

    /// Store-side details beyond the manifest's.
    fn details(&self, store: &Path, ctx: &Ctx) -> Vec<(String, String)> {
        let mut out = units::manifest_details(&self.manifest, self.archive_bytes, &store.join(&self.file), ctx.now_ms);
        out.push(("pushed".into(), units::when(&self.pushed.at, ctx.now_ms)));
        out.push(("pushed from".into(), format!("{}\n{}", self.pushed.root, self.pushed.system.describe())));
        out.push(("restores into".into(), ctx.roots.of(self.manifest.harness).display().to_string()));
        let restores = if self.restores.is_empty() {
            "never".to_string()
        } else {
            self.restores
                .iter()
                .map(|r| format!("{} · {} · {}", r.at, r.system.host, r.root))
                .collect::<Vec<_>>()
                .join("\n")
        };
        out.push(("restored".into(), restores));
        out
    }
}

/// An open store; mutating commands hold its lock.
struct Store {
    dir: PathBuf,
    index: Index,
    _lock: Option<File>,
}

impl Store {
    fn open(dir: &Path) -> Result<Store> {
        let path = dir.join(INDEX);
        let bytes = fs::read(&path).with_context(|| {
            format!("{} is not a store; `pi-transfuse vault init {}` makes it one", dir.display(), dir.display())
        })?;
        let index: Index = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}; `pi-transfuse vault reindex` rebuilds it", path.display()))?;
        if index.format != FORMAT || index.version != VERSION {
            bail!("{} is not a {FORMAT} version {VERSION}", path.display());
        }
        Ok(Store { dir: dir.to_path_buf(), index, _lock: None })
    }

    fn open_locked(dir: &Path) -> Result<Store> {
        let lock = lock(dir)?;
        let mut store = Store::open(dir)?;
        store._lock = lock;
        Ok(store)
    }

    fn save(&mut self, ctx: &Ctx) -> Result<()> {
        write_index(&self.dir, &mut self.index, ctx)
    }

    /// An entry holding exactly this session's content.
    fn holds(&self, archive: &Archive) -> Option<&Entry> {
        self.index.archives.iter().find(|e| {
            e.manifest.harness == archive.manifest.harness
                && e.manifest.id == archive.manifest.id
                && e.content_sha256 == archive.content_sha256
        })
    }

    fn label(&self) -> String {
        format!("{} ({})", self.index.name, self.dir.display())
    }
}

fn write_index(dir: &Path, index: &mut Index, ctx: &Ctx) -> Result<()> {
    index.updated = convert::iso(ctx.now_ms);
    index.updated_on = ctx.system.clone();
    let path = dir.join(INDEX);
    if path.is_file() {
        let _ = fs::copy(&path, dir.join(PREVIOUS));
    }
    relocate::write_atomic(&path, &serde_json::to_vec_pretty(index)?)
}

/// Takes the store's lock, or fails while another run holds it. A POSIX record
/// lock: child processes never inherit it, and network mounts honor it more
/// often than `flock`. Filesystems without locks, as some cloud mounts are, go
/// unguarded.
#[cfg(unix)]
fn lock(dir: &Path) -> Result<Option<File>> {
    use std::os::unix::io::AsRawFd;
    let path = dir.join(LOCK);
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    // SAFETY: all-zero is a valid `flock`; the fields set below make it a
    // whole-file write lock.
    let mut request: libc::flock = unsafe { std::mem::zeroed() };
    request.l_type = libc::F_WRLCK as libc::c_short;
    request.l_whence = libc::SEEK_SET as libc::c_short;
    // SAFETY: the descriptor belongs to `file`, which outlives the call, and
    // `request` is a valid `flock` for F_SETLK to read.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &request) } == 0 {
        return Ok(Some(file));
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EACCES | libc::EAGAIN) => bail!("{} is in use by another pi-transfuse run", dir.display()),
        _ => Ok(None),
    }
}

#[cfg(not(unix))]
fn lock(_: &Path) -> Result<Option<File>> {
    Ok(None)
}

fn new_index(ctx: &Ctx, dir: &Path, name: String) -> Index {
    let mut hasher = Sha256::new();
    hasher.update(ctx.system.host.as_bytes());
    hasher.update(dir.to_string_lossy().as_bytes());
    hasher.update(std::process::id().to_le_bytes());
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    hasher.update(nanos.to_le_bytes());
    let now = convert::iso(ctx.now_ms);
    Index {
        format: FORMAT.to_string(),
        version: VERSION,
        id: archive::hex(&hasher.finalize())[..16].to_string(),
        name,
        created: now.clone(),
        created_on: ctx.system.clone(),
        updated: now,
        updated_on: ctx.system.clone(),
        archives: Vec::new(),
    }
}

fn folder_name(dir: &Path) -> String {
    fs::canonicalize(dir)
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "store".to_string())
}

fn init(ctx: &Ctx, dir: &Path, name: Option<String>) -> Result<i32> {
    if !dir.is_dir() {
        bail!("{} is not a directory; create it first", dir.display());
    }
    if dir.join(INDEX).exists() {
        bail!("{} is already a store", dir.display());
    }
    let mut index = new_index(ctx, dir, name.unwrap_or_else(|| folder_name(dir)));
    fs::create_dir_all(dir.join(ARCHIVES)).with_context(|| format!("create {}", dir.join(ARCHIVES).display()))?;
    write_index(dir, &mut index, ctx)?;
    println!("store {:?} ({}) ready at {}", index.name, index.id, dir.display());
    Ok(0)
}

fn info(_ctx: &Ctx, dir: &Path) -> Result<i32> {
    let store = Store::open(dir)?;
    let ix = &store.index;
    let sessions: HashSet<(Side, &str)> = ix.archives.iter().map(|e| (e.manifest.harness, e.manifest.id.as_str())).collect();
    let original: u64 = ix.archives.iter().map(|e| e.manifest.bytes).sum();
    let packed: u64 = ix.archives.iter().map(|e| e.archive_bytes).sum();
    println!("store      {}", fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()).display());
    println!("name       {}", ix.name);
    println!("id         {}", ix.id);
    println!("created    {} on {}", ix.created, ix.created_on.describe());
    println!("updated    {} on {}", ix.updated, ix.updated_on.describe());
    println!(
        "holds      {} session(s) in {} archive(s) · {} packed from {}",
        sessions.len(),
        ix.archives.len(),
        human_size(packed),
        human_size(original)
    );
    let mut harnesses: BTreeMap<&str, usize> = BTreeMap::new();
    for e in &ix.archives {
        *harnesses.entry(e.manifest.harness.key()).or_default() += 1;
    }
    let harnesses: Vec<String> = harnesses.iter().map(|(k, n)| format!("{k} {n}")).collect();
    println!("harnesses  {}", if harnesses.is_empty() { "—".to_string() } else { harnesses.join(" · ") });

    let mut origins: BTreeMap<(&str, &str, &str), (usize, u64)> = BTreeMap::new();
    for e in &ix.archives {
        let key = (e.pushed.system.host.as_str(), e.pushed.system.user.as_str(), e.pushed.root.as_str());
        let slot = origins.entry(key).or_default();
        slot.0 += 1;
        slot.1 += e.archive_bytes;
    }
    if !origins.is_empty() {
        println!("\norigins");
        for ((host, user, root), (count, bytes)) in origins {
            println!("  {:>9}  {count:>4} archive(s)  {user}@{host}  {root}", human_size(bytes));
        }
    }

    struct Project<'a> {
        sessions: BTreeSet<&'a str>,
        archives: usize,
        original: u64,
        packed: u64,
        last_used: &'a str,
    }
    let mut projects: BTreeMap<&str, Project> = BTreeMap::new();
    for e in &ix.archives {
        let p = projects.entry(e.manifest.cwd.as_str()).or_insert(Project {
            sessions: BTreeSet::new(),
            archives: 0,
            original: 0,
            packed: 0,
            last_used: "",
        });
        p.sessions.insert(&e.manifest.id);
        p.archives += 1;
        p.original += e.manifest.bytes;
        p.packed += e.archive_bytes;
        p.last_used = p.last_used.max(e.manifest.last_used.as_str());
    }
    let mut projects: Vec<(&str, Project)> = projects.into_iter().collect();
    projects.sort_by_key(|(_, p)| std::cmp::Reverse(p.packed));
    if !projects.is_empty() {
        println!("\nprojects");
        for (cwd, p) in projects {
            println!(
                "  {:>9} of {:>9}  {:>4} session(s)  last used {}  {}",
                human_size(p.packed),
                human_size(p.original),
                p.sessions.len(),
                p.last_used.get(..10).unwrap_or(p.last_used),
                units::tilde(cwd)
            );
        }
    }
    Ok(0)
}

fn list(ctx: &Ctx, args: &ListArgs) -> Result<i32> {
    let store = Store::open(&args.store)?;
    let sides = args.scope.sides();
    let mut entries: Vec<&Entry> = store
        .index
        .archives
        .iter()
        .filter(|e| sides.contains(&e.manifest.harness) && args.scope.admits(&e.search()))
        .collect();
    entries.sort_by(|a, b| a.manifest.cwd.cmp(&b.manifest.cwd).then(b.manifest.last_used.cmp(&a.manifest.last_used)));
    if args.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(0);
    }
    if !args.long {
        units::print_header();
    }
    for e in &entries {
        let m = &e.manifest;
        if args.long {
            println!("── {} ──", if m.title.is_empty() { &m.id } else { &m.title });
            units::print_details(&e.details(&store.dir, ctx));
            println!();
        } else {
            let days = units::age_days(ctx.now_ms, m.last_used_ms());
            units::print_line(days, &human_size(m.bytes), &human_size(e.archive_bytes), m.harness, &m.id, &m.cwd, &m.title);
        }
    }
    let packed: u64 = entries.iter().map(|e| e.archive_bytes).sum();
    println!("{} archive(s) in {} · {}", entries.len(), store.label(), human_size(packed));
    Ok(0)
}

/// Store path of an archive: by harness and bucket, stamped with its pack time
/// so successive copies of one session sit side by side.
fn store_file(harness: Side, bucket: &str, name: &str, packed_ms: i64, extension: &str) -> String {
    let stamp = chrono::Utc
        .timestamp_millis_opt(packed_ms)
        .single()
        .map_or_else(|| "unknown".to_string(), |t| t.format("%Y%m%dT%H%M%SZ").to_string());
    format!("{ARCHIVES}/{}/{bucket}/{name}.{stamp}{extension}", harness.key())
}

/// A pushable session: live, or packed in place.
enum Item<'a> {
    Live(&'a Unit),
    Packed(&'a Local),
}

fn push(ctx: &Ctx, args: &PushArgs) -> Result<i32> {
    let compression = args.compress.resolve()?;
    let mut store = Store::open_locked(&args.store)?;
    let sides = args.scope.sides();
    let live = units::discover(&ctx.roots, &sides);
    let packed = compact::discover(&ctx.roots, &sides);
    let (items, candidates) = push_candidates(ctx, &live, &packed);
    let choice = Choice {
        title: format!("vault push → {} · {compression}", store.index.name),
        verb: "push",
        threshold: Some(args.older_than),
    };
    let chosen = units::choose(candidates, &args.act, &args.scope, choice)?;
    if chosen.is_empty() {
        println!("nothing chosen");
        return Ok(0);
    }
    let live_chosen: Vec<&Unit> = chosen
        .iter()
        .filter_map(|&i| match items[i] {
            Item::Live(unit) => Some(unit),
            Item::Packed(_) => None,
        })
        .collect();
    let held = units::held_pi(ctx, &live_chosen);
    let mut report = Report::new(Verb::Push, args.act.dry_run);
    let mut unsaved = 0;
    for i in chosen {
        let (harness, id, title, bytes) = match items[i] {
            Item::Live(u) => (u.harness, &u.id, &u.title, u.bytes),
            Item::Packed(l) => (l.manifest.harness, &l.manifest.id, &l.manifest.title, l.archive_bytes),
        };
        let before = store.index.archives.len();
        let (status, sizes, freed) = if matches!(items[i], Item::Live(u) if held.contains(&u.id)) {
            (Status::Held, human_size(bytes), 0)
        } else if args.act.dry_run {
            (Status::Would, human_size(bytes), if args.keep { 0 } else { bytes })
        } else {
            match items[i] {
                Item::Live(unit) => push_live(ctx, &mut store, unit, compression, args.keep),
                Item::Packed(local) => push_local(ctx, &mut store, local, args.keep),
            }
        };
        report.freed += freed;
        report.line(&status, harness, id, title, &sizes);
        unsaved += store.index.archives.len() - before;
        if unsaved >= SAVE_EVERY {
            store.save(ctx).context(REINDEX_HINT)?;
            unsaved = 0;
        }
    }
    if unsaved > 0 {
        store.save(ctx).context(REINDEX_HINT)?;
    }
    println!("store {}", store.label());
    Ok(report.finish())
}

/// Adds a checked archive to the in-memory index; `push` writes it out in
/// batches. An archive that never reaches the index is still found by
/// `vault reindex`, since every archive describes itself.
fn record(ctx: &Ctx, store: &mut Store, rel: String, archive: &Archive, root: &Path) {
    store.index.archives.push(Entry::new(rel, archive, Place::here(ctx, root)));
}


/// Packs a live session straight into the store, then removes it locally
/// unless `keep`. Returns the status, sizes, and local bytes freed.
fn push_live(
    ctx: &Ctx,
    store: &mut Store,
    unit: &Unit,
    compression: archive::Compression,
    keep: bool,
) -> (Status, String, u64) {
    let root = ctx.roots.of(unit.harness);
    let mut sizes = human_size(unit.bytes);
    let rel = store_file(unit.harness, &unit.bucket, &unit.name, ctx.now_ms, compression.algo.extension());
    let dest = store.dir.join(&rel);
    let packed = match archive::pack(root, unit, &dest, compression, &ctx.system, ctx.now_ms) {
        Ok(packed) => packed,
        Err(err) => return (Status::from_error(err), sizes, 0),
    };
    sizes = format!("{} → {}", human_size(unit.bytes), human_size(packed.archive_bytes));
    let status = if store.holds(&packed).is_some() {
        let _ = fs::remove_file(&packed.path);
        Status::Stored
    } else {
        record(ctx, store, rel, &packed, root);
        Status::Done
    };
    if keep {
        return (status, sizes, 0);
    }
    match units::remove_members(root, &unit.members) {
        Ok(()) => (status, sizes, unit.bytes),
        Err(err) => (Status::Failed(format!("stored, but {err:#}")), sizes, 0),
    }
}

/// Copies an archive packed in place into the store as it is, then removes
/// the local archive unless `keep`.
fn push_local(ctx: &Ctx, store: &mut Store, local: &Local, keep: bool) -> (Status, String, u64) {
    let m = &local.manifest;
    let root = ctx.roots.of(m.harness);
    let sizes = human_size(local.archive_bytes);
    let checked = match archive::verify(&local.path) {
        Ok(checked) => checked,
        Err(err) => return (Status::Failed(format!("{err:#}")), sizes, 0),
    };
    let status = if store.holds(&checked).is_some() {
        Status::Stored
    } else {
        let packed_ms = convert::from_iso(&m.packed_at).unwrap_or(0);
        let rel = store_file(m.harness, &m.bucket, &m.name, packed_ms, m.compression.algo.extension());
        let dest = store.dir.join(&rel);
        if dest.symlink_metadata().is_ok() {
            return (Status::Conflict(format!("{} exists", dest.display())), sizes, 0);
        }
        let tmp = archive::partial(&dest);
        let copied = dest
            .parent()
            .map_or(Ok(()), |p| fs::create_dir_all(p).with_context(|| format!("create {}", p.display())))
            .and_then(|()| relocate::copy_file(&local.path, &tmp))
            .and_then(|()| fs::rename(&tmp, &dest).with_context(|| format!("rename into {}", dest.display())));
        if let Err(err) = copied {
            let _ = fs::remove_file(&tmp);
            return (Status::Failed(format!("{err:#}")), sizes, 0);
        }
        record(ctx, store, rel, &checked, root);
        Status::Done
    };
    if keep {
        return (status, sizes, 0);
    }
    match fs::remove_file(&local.path) {
        Ok(()) => (status, sizes, local.archive_bytes),
        Err(err) => (Status::Failed(format!("stored, but could not remove {}: {err}", local.path.display())), sizes, 0),
    }
}

fn pull(ctx: &Ctx, args: &PullArgs) -> Result<i32> {
    let mut store = Store::open_locked(&args.store)?;
    let sides = args.scope.sides();
    let (order, candidates) = pull_candidates(ctx, &store, &sides);
    let choice = Choice { title: format!("vault pull ← {}", store.index.name), verb: "pull", threshold: None };
    let chosen: Vec<usize> =
        units::choose(candidates, &args.act, &args.scope, choice)?.into_iter().map(|k| order[k]).collect();
    if chosen.is_empty() {
        println!("nothing chosen");
        return Ok(0);
    }
    let mut report = Report::new(Verb::Pull, args.act.dry_run);
    let mut drop: BTreeSet<usize> = BTreeSet::new();
    let mut changed = false;
    for i in chosen {
        let e = store.index.archives[i].clone();
        let m = &e.manifest;
        let root = ctx.roots.of(m.harness);
        let sizes = format!("{} → {}", human_size(e.archive_bytes), human_size(m.bytes));
        let taken = m.occupied(root);
        let status = if !taken.is_empty() {
            Status::Conflict(format!("{} already exists", taken.join(", ")))
        } else if args.act.dry_run {
            Status::Would
        } else {
            match archive::extract(&store.dir.join(&e.file), root) {
                Ok(_) => {
                    store.index.archives[i].restores.push(Place::here(ctx, root));
                    changed = true;
                    if args.remove {
                        drop.insert(i);
                    }
                    Status::Done
                }
                Err(err) => Status::from_error(err),
            }
        };
        if status.ok() {
            report.restored += m.bytes;
        }
        report.line(&status, m.harness, &m.id, &m.title, &sizes);
    }
    if changed {
        forget(&mut store, &drop);
        store.save(ctx)?;
    }
    Ok(report.finish())
}

/// What `push` offers: every live session, then every session packed in place.
fn push_candidates<'a>(ctx: &Ctx, live: &'a [Unit], packed: &'a [Local]) -> (Vec<Item<'a>>, Vec<Candidate>) {
    let mut items = Vec::new();
    let mut candidates = Vec::new();
    for unit in live {
        let row = units::unit_row(unit, ctx.roots.of(unit.harness), ctx.now_ms);
        candidates.push(Candidate { id: unit.id.clone(), path: None, row });
        items.push(Item::Live(unit));
    }
    for local in packed {
        let mut candidate = compact::local_row(ctx, local);
        candidate.row.blocked = None;
        candidates.push(candidate);
        items.push(Item::Packed(local));
    }
    (items, candidates)
}

/// What `pull` offers, as index positions with their rows: newest copy first,
/// so naming a session pulls its latest; a session present locally is blocked.
fn pull_candidates(ctx: &Ctx, store: &Store, sides: &[Side]) -> (Vec<usize>, Vec<Candidate>) {
    let archives = &store.index.archives;
    let mut order: Vec<usize> = (0..archives.len()).filter(|&i| sides.contains(&archives[i].manifest.harness)).collect();
    order.sort_by(|&a, &b| archives[b].manifest.packed_at.cmp(&archives[a].manifest.packed_at));
    let candidates = order
        .iter()
        .map(|&i| {
            let e = &archives[i];
            let path = store.dir.join(&e.file);
            let mut row = units::archive_row(&e.manifest, e.archive_bytes, &path, ctx.now_ms, Vec::new());
            row.details = e.details(&store.dir, ctx);
            let taken = e.manifest.occupied(ctx.roots.of(e.manifest.harness));
            if !taken.is_empty() {
                row.blocked = Some(format!("present locally: {}", taken.join(", ")));
            }
            Candidate { id: e.manifest.id.clone(), path: Some(path), row }
        })
        .collect();
    (order, candidates)
}

/// Deletes the archives at these index positions, and their entries.
fn forget(store: &mut Store, drop: &BTreeSet<usize>) {
    for &i in drop {
        let file = store.dir.join(&store.index.archives[i].file);
        if let Err(err) = fs::remove_file(&file) {
            eprintln!("note: could not remove {}: {err}", file.display());
        }
    }
    let mut position = 0;
    store.index.archives.retain(|_| {
        position += 1;
        !drop.contains(&(position - 1))
    });
}

fn reindex(ctx: &Ctx, dir: &Path) -> Result<i32> {
    let _lock = lock(dir)?;
    let path = dir.join(INDEX);
    let previous: Option<Index> = [INDEX, PREVIOUS]
        .iter()
        .find_map(|name| serde_json::from_slice(&fs::read(dir.join(name)).ok()?).ok());
    if previous.is_none() {
        if !path.exists() && !dir.join(ARCHIVES).is_dir() {
            bail!("{} is not a store", dir.display());
        }
        eprintln!("note: no readable index; rebuilding {} from the archives under a new identity", path.display());
    }
    let mut known: BTreeMap<String, Entry> = previous
        .as_ref()
        .map(|ix| ix.archives.iter().map(|e| (e.file.clone(), e.clone())).collect())
        .unwrap_or_default();
    let mut files: Vec<PathBuf> = WalkDir::new(dir.join(ARCHIVES))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.file_name().to_str().is_some_and(archive::is_archive_name))
        .map(walkdir::DirEntry::into_path)
        .collect();
    files.sort();
    let (mut kept, mut added, mut corrupt) = (0, 0, 0);
    let mut archives = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(dir)
            .map(|r| r.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
            .unwrap_or_default();
        let previous = known.remove(&rel);
        match archive::verify(&file) {
            Ok(checked) => match previous {
                Some(entry) if entry.archive_sha256 == checked.archive_sha256 => {
                    kept += 1;
                    archives.push(entry);
                }
                _ => {
                    added += 1;
                    println!("added    {rel}");
                    let pushed = Place {
                        at: checked.manifest.packed_at.clone(),
                        root: checked.manifest.root.clone(),
                        system: checked.manifest.packed_on.clone(),
                    };
                    archives.push(Entry::new(rel, &checked, pushed));
                }
            },
            Err(err) => {
                corrupt += 1;
                println!("corrupt  {rel}  ({err:#})");
            }
        }
    }
    for rel in known.keys() {
        println!("missing  {rel}");
    }
    let mut index = previous.unwrap_or_else(|| new_index(ctx, dir, folder_name(dir)));
    index.archives = archives;
    write_index(dir, &mut index, ctx)?;
    println!(
        "{} archive(s) indexed · {kept} unchanged · {added} added · {} missing · {corrupt} corrupt",
        kept + added,
        known.len()
    );
    Ok(i32::from(corrupt > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::Algo;
    use crate::units::tests::{tree, Scratch};

    fn act(sessions: &[&str]) -> Act {
        Act { sessions: sessions.iter().map(|s| s.to_string()).collect(), yes: true, dry_run: false }
    }

    fn scope() -> Scope {
        Scope { harness: Vec::new(), filter: None }
    }

    fn push_args(store: &Path, sessions: &[&str]) -> PushArgs {
        PushArgs {
            store: store.to_path_buf(),
            act: act(sessions),
            scope: scope(),
            older_than: "0d".parse().unwrap(),
            compress: CompressArgs { algo: Algo::Zstd, level: Some(3), threads: Some(0) },
            keep: false,
        }
    }

    fn pull_args(store: &Path, sessions: &[&str], remove: bool) -> PullArgs {
        PullArgs { store: store.to_path_buf(), act: act(sessions), scope: scope(), remove }
    }

    fn new_store(scratch: &Scratch, ctx: &Ctx) -> PathBuf {
        let store = scratch.dir.join("store");
        fs::create_dir_all(&store).unwrap();
        init(ctx, &store, Some("test".into())).unwrap();
        store
    }

    #[test]
    fn push_moves_a_session_away_and_pull_puts_it_back() {
        let scratch = Scratch::new("vault");
        let ctx = scratch.ctx();
        let store = new_store(&scratch, &ctx);
        scratch.pi_session("one", "/work/a");
        let before = tree(&scratch.roots.pi);

        assert_eq!(push(&ctx, &push_args(&store, &["one"])).unwrap(), 0);
        assert!(units::discover(&ctx.roots, &[Side::Pi]).is_empty());
        let index = Store::open(&store).unwrap().index;
        assert_eq!(index.archives.len(), 1);
        let entry = &index.archives[0];
        assert_eq!(entry.manifest.id, "one");
        assert_eq!(entry.manifest.cwd, "/work/a");
        assert_eq!(entry.pushed.root, ctx.roots.pi.display().to_string());
        assert!(store.join(&entry.file).is_file());
        assert!(entry.file.starts_with(&format!("archives/pi/{}/", entry.manifest.bucket)));

        assert_eq!(pull(&ctx, &pull_args(&store, &["one"], false)).unwrap(), 0);
        assert_eq!(tree(&scratch.roots.pi), before);
        let index = Store::open(&store).unwrap().index;
        assert_eq!(index.archives.len(), 1, "pull keeps the store's copy");
        assert_eq!(index.archives[0].restores.len(), 1);

        assert_eq!(pull(&ctx, &pull_args(&store, &["one"], false)).unwrap(), 1, "a live copy is a conflict");
    }

    #[test]
    fn pushing_an_unchanged_session_again_stores_nothing_new() {
        let scratch = Scratch::new("vault-dedup");
        let ctx = scratch.ctx();
        let store = new_store(&scratch, &ctx);
        scratch.pi_session("one", "/work/a");
        push(&ctx, &push_args(&store, &["one"])).unwrap();
        pull(&ctx, &pull_args(&store, &["one"], false)).unwrap();

        let mut later = scratch.ctx();
        later.now_ms += 60_000;
        assert_eq!(push(&later, &push_args(&store, &["one"])).unwrap(), 0);
        assert!(units::discover(&ctx.roots, &[Side::Pi]).is_empty(), "the local copy still leaves");
        let index = Store::open(&store).unwrap().index;
        assert_eq!(index.archives.len(), 1);
        let files = WalkDir::new(store.join(ARCHIVES)).into_iter().filter_map(Result::ok).filter(|e| e.file_type().is_file()).count();
        assert_eq!(files, 1);
    }

    #[test]
    fn a_session_packed_in_place_moves_into_the_store_as_is() {
        let scratch = Scratch::new("vault-local");
        let ctx = scratch.ctx();
        let store = new_store(&scratch, &ctx);
        scratch.pi_session("one", "/work/a");
        let before = tree(&scratch.roots.pi);
        let pack = compact::Command::Pack(compact::PackArgs {
            act: act(&["one"]),
            scope: scope(),
            older_than: "0d".parse().unwrap(),
            compress: CompressArgs { algo: Algo::Gzip, level: Some(1), threads: None },
        });
        compact::run(&ctx, pack).unwrap();
        let local = compact::discover(&ctx.roots, &[Side::Pi]).remove(0);
        let local_sha = archive::hex(&relocate::digest(&local.path).unwrap().1);

        assert_eq!(push(&ctx, &push_args(&store, &["one"])).unwrap(), 0);
        assert!(compact::discover(&ctx.roots, &[Side::Pi]).is_empty());
        let index = Store::open(&store).unwrap().index;
        assert_eq!(index.archives[0].archive_sha256, local_sha);
        assert!(index.archives[0].file.ends_with(".tar.gz"));

        pull(&ctx, &pull_args(&store, &["one"], true)).unwrap();
        assert_eq!(tree(&scratch.roots.pi), before);
        assert!(Store::open(&store).unwrap().index.archives.is_empty(), "--remove drops the store's copy");
    }

    #[test]
    fn reindex_rebuilds_a_lost_index_from_the_archives() {
        let scratch = Scratch::new("vault-reindex");
        let ctx = scratch.ctx();
        let store = new_store(&scratch, &ctx);
        scratch.pi_session("one", "/work/a");
        scratch.pi_session("two", "/work/b");
        push(&ctx, &push_args(&store, &["one", "two"])).unwrap();
        let original = Store::open(&store).unwrap().index;
        fs::remove_file(store.join(INDEX)).unwrap();

        assert_eq!(reindex(&ctx, &store).unwrap(), 0);
        let rebuilt = Store::open(&store).unwrap().index;
        let ids = |ix: &Index| ix.archives.iter().map(|e| (e.manifest.id.clone(), e.content_sha256.clone())).collect::<BTreeSet<_>>();
        assert_eq!(ids(&rebuilt), ids(&original));

        // A damaged archive is reported and left out.
        let victim = store.join(&rebuilt.archives[0].file);
        let bytes = fs::read(&victim).unwrap();
        fs::write(&victim, &bytes[..bytes.len() / 2]).unwrap();
        assert_eq!(reindex(&ctx, &store).unwrap(), 1);
        assert_eq!(Store::open(&store).unwrap().index.archives.len(), 1);
    }

    #[test]
    fn a_folder_must_be_initialized_before_pushing() {
        let scratch = Scratch::new("vault-uninit");
        let ctx = scratch.ctx();
        scratch.pi_session("one", "/work/a");
        let plain = scratch.dir.join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(push(&ctx, &push_args(&plain, &["one"])).is_err());
        assert_eq!(units::discover(&ctx.roots, &[Side::Pi]).len(), 1);
        assert!(init(&ctx, &plain, None).is_ok());
        assert!(init(&ctx, &plain, None).is_err(), "a store is initialized once");
    }
}
