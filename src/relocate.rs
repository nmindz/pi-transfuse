//! Moves every Pi session recorded under one directory to the matching path
//! under another, together with the OMP state that points at those sessions.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{paths, pi};

/// What the user asked for.
pub struct Request {
    pub old: PathBuf,
    pub new: PathBuf,
    pub dry_run: bool,
    pub wait: bool,
    pub move_dir: bool,
}

/// Where the OMP state lives and how to reach a running omp.
pub struct Env {
    pub pi_root: PathBuf,
    pub omp_home: PathBuf,
    pub omp: Option<PathBuf>,
    pub sqlite: Option<PathBuf>,
    pub poll: Duration,
}

impl Env {
    /// The environment the CLI runs in.
    pub fn from_system() -> Env {
        Env {
            pi_root: paths::pi_root(),
            omp_home: paths::omp_home(),
            omp: which("omp"),
            sqlite: which("sqlite3"),
            poll: Duration::from_secs(1),
        }
    }
}

/// Outcome of one selected session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Moved,
    WouldMove,
    Held,
    Conflict,
    Error(String),
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Moved => "moved",
            Status::WouldMove => "would-move",
            Status::Held => "held",
            Status::Conflict => "conflict",
            Status::Error(_) => "error",
        }
    }
}

/// One selected session and what happened to it.
#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub old_cwd: String,
    pub new_cwd: String,
    pub status: Status,
}

impl Item {
    fn bucket(path: &Path) -> String {
        path.parent()
            .and_then(Path::file_name)
            .map(|b| b.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// Artifact directory beside a transcript: its file stem.
    fn artifacts(path: &Path) -> PathBuf {
        path.with_extension("")
    }

    /// Lock sidecar beside a transcript: `.<file name>.lock.os`.
    fn lock(path: &Path) -> PathBuf {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        path.with_file_name(format!(".{name}.lock.os"))
    }

    fn relocated(&self) -> bool {
        matches!(self.status, Status::Moved | Status::WouldMove)
    }
}

/// Rows or files each index kind changed, or would change.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexCounts {
    pub breadcrumbs: usize,
    pub custom_markers: usize,
    pub history: usize,
    pub composer: usize,
}

/// Everything a run did.
#[derive(Debug)]
pub struct Report {
    pub items: Vec<Item>,
    pub indexes: IndexCounts,
    pub dry_run: bool,
}

impl Report {
    /// 0 when every selected session moved or would move, else 1.
    pub fn exit_code(&self) -> i32 {
        if self.items.iter().all(Item::relocated) { 0 } else { 1 }
    }

    pub fn print(&self) {
        for item in &self.items {
            let reason = match &item.status {
                Status::Error(reason) => format!("  ({reason})"),
                _ => String::new(),
            };
            println!(
                "{:<10} {}  {}  {} → {}{reason}",
                item.status.label(),
                item.id,
                crate::clip(if item.title.is_empty() { "(untitled)" } else { &item.title }, 48),
                Item::bucket(&item.old_path),
                Item::bucket(&item.new_path),
            );
        }
        let idx = &self.indexes;
        println!("breadcrumbs          {}", idx.breadcrumbs);
        println!("custom-session-files {}", idx.custom_markers);
        println!("history.db           {}", idx.history);
        println!("composer.db          {}", idx.composer);
        let count = |f: fn(&Status) -> bool| self.items.iter().filter(|i| f(&i.status)).count();
        let moved = count(|s| matches!(s, Status::Moved | Status::WouldMove));
        println!(
            "{} selected · {} {} · {} held · {} conflict · {} error",
            self.items.len(),
            moved,
            if self.dry_run { "would-move" } else { "moved" },
            count(|s| *s == Status::Held),
            count(|s| *s == Status::Conflict),
            count(|s| matches!(s, Status::Error(_))),
        );
    }
}

/// Runs a relocation. An `Err` is an argument or environment error.
pub fn run(req: &Request, env: &Env) -> Result<Report> {
    let old = absolute(&req.old)?;
    let new_lexical = absolute(&req.new)?;
    if old == new_lexical {
        bail!("old and new directory are the same");
    }
    if new_lexical.starts_with(&old) {
        bail!("{} lies inside {}", new_lexical.display(), old.display());
    }
    if req.move_dir {
        move_dir(&old, &new_lexical, req.dry_run)?;
    } else if !new_lexical.is_dir() {
        bail!("{} does not exist; pass --move-dir to rename the old directory", new_lexical.display());
    }
    let new = resolve(&new_lexical);
    let new_str = path_str(&new)?;

    // A session may record the old directory as typed or with symlinks resolved.
    let mut olds = vec![path_str(&old)?];
    if let Ok(canon) = fs::canonicalize(&old) {
        let canon = path_str(&canon)?;
        if !olds.contains(&canon) {
            olds.push(canon);
        }
    }
    let remap = |path: &str| olds.iter().find_map(|o| remap(path, o, &new_str));

    let mut items: Vec<Item> = pi::discover(&env.pi_root)
        .into_iter()
        .filter(|s| s.path.parent().and_then(Path::parent) == Some(env.pi_root.as_path()))
        .filter_map(|s| {
            let new_cwd = remap(&s.cwd)?;
            let new_path = env
                .pi_root
                .join(paths::encode_pi_bucket(&new_cwd))
                .join(s.path.file_name()?);
            Some(Item {
                id: s.id,
                title: s.title,
                old_path: s.path,
                new_path,
                old_cwd: s.cwd,
                new_cwd,
                status: Status::WouldMove,
            })
        })
        .collect();
    items.sort_by(|a, b| a.old_path.cmp(&b.old_path));

    if items.is_empty() {
        return Ok(Report { items, indexes: IndexCounts::default(), dry_run: req.dry_run });
    }
    let omp = env.omp.as_deref().ok_or_else(|| anyhow!("omp not found on PATH"))?;
    let needs_sqlite = [history_db(env), composer_db(env)].iter().any(|db| db.is_file());
    if needs_sqlite && env.sqlite.is_none() {
        bail!("sqlite3 not found on PATH");
    }

    let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    let held = lease(omp, &ids)?;
    let mut old_buckets = BTreeSet::new();
    for item in &mut items {
        let held_now = held.contains(&item.id);
        if held_now && !req.wait {
            item.status = Status::Held;
            continue;
        }
        if conflicts(item) {
            item.status = Status::Conflict;
            continue;
        }
        if req.dry_run {
            continue;
        }
        if held_now {
            eprintln!("waiting for the omp holding {} to exit…", item.id);
            loop {
                std::thread::sleep(env.poll);
                if !lease(omp, std::slice::from_ref(&item.id))?.contains(&item.id) {
                    break;
                }
            }
        }
        let extra: Vec<&str> = olds.iter().map(String::as_str).collect();
        item.status = match move_session(item, &extra, &new_str) {
            Ok(()) => {
                if let Some(bucket) = item.old_path.parent() {
                    old_buckets.insert(bucket.to_path_buf());
                }
                Status::Moved
            }
            Err(err) => Status::Error(format!("{err:#}")),
        };
    }
    for bucket in old_buckets {
        // Fails, as intended, while anything is left in it.
        let _ = fs::remove_dir(bucket);
    }

    let indexes = update_indexes(&items, env, req.dry_run)?;
    Ok(Report { items, indexes, dry_run: req.dry_run })
}

/// `path` with `old` replaced by `new` when it is `old` or lies under it.
fn remap(path: &str, old: &str, new: &str) -> Option<String> {
    let rest = path.strip_prefix(old)?;
    if rest.is_empty() || (rest.starts_with('/') && !old.ends_with('/')) {
        Some(format!("{new}{rest}"))
    } else {
        None
    }
}

fn path_str(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{} is not valid UTF-8", path.display()))
}

/// Absolute path with `.` and `..` folded, without touching the filesystem.
fn absolute(path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().context("read current directory")?.join(path)
    };
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Resolves symlinks in the deepest existing ancestor, as OMP does for a cwd.
fn resolve(path: &Path) -> PathBuf {
    let mut tail: Vec<OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        if let Ok(canon) = fs::canonicalize(&cursor) {
            return tail.iter().rev().fold(canon, |acc, part| acc.join(part));
        }
        match (cursor.file_name().map(OsString::from), cursor.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cursor = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Finds an executable on `PATH`.
pub(crate) fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Renames the project directory itself.
fn move_dir(old: &Path, new: &Path, dry_run: bool) -> Result<()> {
    if !old.exists() && new.is_dir() {
        // Already renamed by an earlier run.
        return Ok(());
    }
    if !old.is_dir() {
        bail!("{} is not a directory", old.display());
    }
    if new.exists() {
        let empty = new.is_dir()
            && fs::read_dir(new).with_context(|| format!("read {}", new.display()))?.next().is_none();
        if !empty {
            bail!("{} exists and is not an empty directory", new.display());
        }
    }
    let parent = new.parent().ok_or_else(|| anyhow!("{} has no parent", new.display()))?;
    let parent_meta = fs::metadata(parent).with_context(|| format!("stat {}", parent.display()))?;
    if device(&fs::metadata(old)?) != device(&parent_meta) {
        bail!("{} and {} are on different filesystems", old.display(), new.display());
    }
    if dry_run {
        return Ok(());
    }
    if new.exists() {
        fs::remove_dir(new).with_context(|| format!("remove empty {}", new.display()))?;
    }
    fs::rename(old, new).with_context(|| format!("rename {} to {}", old.display(), new.display()))
}

#[cfg(unix)]
fn device(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

#[cfg(not(unix))]
fn device(_: &fs::Metadata) -> u64 {
    0
}

/// Session ids a running omp holds; ids omp cannot judge count as held.
pub(crate) fn lease(omp: &Path, ids: &[String]) -> Result<HashSet<String>> {
    let out = Command::new(omp)
        .args(["session", "lease", "--json", "--"])
        .args(ids)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("run {}", omp.display()))?;
    if !out.status.success() {
        bail!("omp session lease failed with {}", out.status);
    }
    let parsed: Value = serde_json::from_slice(&out.stdout).context("parse omp session lease output")?;
    let mut held = HashSet::new();
    for key in ["held", "unknown"] {
        for id in parsed.get(key).and_then(Value::as_array).into_iter().flatten() {
            if let Some(id) = id.as_str() {
                held.insert(id.to_string());
            }
        }
    }
    Ok(held)
}

fn conflicts(item: &Item) -> bool {
    [item.new_path.clone(), Item::artifacts(&item.new_path), Item::lock(&item.new_path)]
        .iter()
        .any(|p| p.symlink_metadata().is_ok())
}

/// A step already taken for one session, undone in reverse on failure.
enum Undo {
    Moved { from: PathBuf, to: PathBuf },
    MadeDir(PathBuf),
    Header { path: PathBuf, line: Vec<u8> },
}

/// Moves one session as a unit, rolling back every step when one fails.
fn move_session(item: &Item, olds: &[&str], new: &str) -> Result<()> {
    let mut undo = Vec::new();
    let result = move_steps(item, olds, new, &mut undo);
    if result.is_err() {
        for step in undo.into_iter().rev() {
            let _ = match step {
                Undo::Moved { from, to } => move_path(&to, &from),
                Undo::MadeDir(dir) => fs::remove_dir(dir).map_err(Into::into),
                Undo::Header { path, line } => replace_header(&path, &line).map(|_| ()),
            };
        }
    }
    result
}

fn move_steps(item: &Item, olds: &[&str], new: &str, undo: &mut Vec<Undo>) -> Result<()> {
    let bucket = item.new_path.parent().ok_or_else(|| anyhow!("destination has no bucket"))?;
    if !bucket.exists() {
        fs::create_dir_all(bucket).with_context(|| format!("create {}", bucket.display()))?;
        undo.push(Undo::MadeDir(bucket.to_path_buf()));
    }
    let remap_all = |path: &str| olds.iter().find_map(|o| remap(path, o, new));

    move_path(&item.old_path, &item.new_path)?;
    undo.push(Undo::Moved { from: item.old_path.clone(), to: item.new_path.clone() });
    injected_failure()?;

    let old_artifacts = Item::artifacts(&item.old_path);
    let new_artifacts = Item::artifacts(&item.new_path);
    if old_artifacts.is_dir() {
        move_path(&old_artifacts, &new_artifacts)?;
        undo.push(Undo::Moved { from: old_artifacts, to: new_artifacts.clone() });
    }
    let old_lock = Item::lock(&item.old_path);
    if old_lock.symlink_metadata().is_ok() {
        let new_lock = Item::lock(&item.new_path);
        move_path(&old_lock, &new_lock)?;
        undo.push(Undo::Moved { from: old_lock, to: new_lock });
    }

    let previous = path_str(&item.old_path)?;
    let line = rewrite_header(&item.new_path, |header| {
        rewrite_dirs(header, &remap_all);
        let list = header
            .entry("previousSessionFiles")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(list) = list {
            if !list.iter().any(|v| v.as_str() == Some(&previous)) {
                list.push(Value::String(previous.clone()));
            }
        }
    })?;
    undo.push(Undo::Header { path: item.new_path.clone(), line });

    if new_artifacts.is_dir() {
        for entry in WalkDir::new(&new_artifacts).into_iter() {
            let entry = entry.with_context(|| format!("walk {}", new_artifacts.display()))?;
            let path = entry.path();
            if !entry.file_type().is_file() || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if !pi::is_session_file(path)? {
                continue;
            }
            let line = rewrite_header(path, |header| rewrite_dirs(header, &remap_all))?;
            undo.push(Undo::Header { path: path.to_path_buf(), line });
        }
    }
    Ok(())
}

/// Remaps `cwd` and every `additionalDirectories` entry under the old directory.
fn rewrite_dirs(header: &mut Map<String, Value>, remap: &dyn Fn(&str) -> Option<String>) {
    if let Some(cwd) = header.get("cwd").and_then(Value::as_str).and_then(remap) {
        header.insert("cwd".into(), Value::String(cwd));
    }
    if let Some(Value::Array(dirs)) = header.get_mut("additionalDirectories") {
        for dir in dirs.iter_mut() {
            if let Some(mapped) = dir.as_str().and_then(remap) {
                *dir = Value::String(mapped);
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_TRANSCRIPT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn injected_failure() -> Result<()> {
    if FAIL_AFTER_TRANSCRIPT.with(|f| f.get()) {
        bail!("injected failure");
    }
    Ok(())
}

#[cfg(not(test))]
fn injected_failure() -> Result<()> {
    Ok(())
}

/// Byte range of the header line, without its newline.
fn header_span(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut start = 0;
    while start < bytes.len() {
        let end = bytes[start..].iter().position(|&b| b == b'\n').map_or(bytes.len(), |p| start + p);
        let line = &bytes[start..end];
        if !line.iter().all(u8::is_ascii_whitespace) {
            let kind = serde_json::from_slice::<Value>(line)
                .ok()
                .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_string));
            match kind.as_deref() {
                Some("title") => {}
                Some("session") => return Some((start, end)),
                _ => return None,
            }
        }
        start = end + 1;
    }
    None
}

/// Applies `edit` to the header, keeping every other byte; returns the old line.
fn rewrite_header(path: &Path, edit: impl FnOnce(&mut Map<String, Value>)) -> Result<Vec<u8>> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let (start, end) = header_span(&bytes).ok_or_else(|| anyhow!("{} has no session header", path.display()))?;
    let mut header: Map<String, Value> =
        serde_json::from_slice(&bytes[start..end]).with_context(|| format!("parse header of {}", path.display()))?;
    edit(&mut header);
    let line = serde_json::to_vec(&Value::Object(header))?;
    splice(path, &bytes, start, end, &line)?;
    Ok(bytes[start..end].to_vec())
}

/// Puts `line` back as the header, for rollback.
fn replace_header(path: &Path, line: &[u8]) -> Result<Vec<u8>> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let (start, end) = header_span(&bytes).ok_or_else(|| anyhow!("{} has no session header", path.display()))?;
    splice(path, &bytes, start, end, line)?;
    Ok(bytes[start..end].to_vec())
}

fn splice(path: &Path, bytes: &[u8], start: usize, end: usize, line: &[u8]) -> Result<()> {
    let mut out = Vec::with_capacity(bytes.len() + line.len());
    out.extend_from_slice(&bytes[..start]);
    out.extend_from_slice(line);
    out.extend_from_slice(&bytes[end..]);
    write_atomic(path, &out)
}

/// Writes through a temp file in the same directory, then renames over `path`.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.pi-transfuse-{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        if let Ok(meta) = fs::metadata(path) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Renames a file or directory, copying and verifying across filesystems.
fn move_path(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if is_cross_device(&err) => copy_verify_remove(from, to),
        Err(err) => Err(err).with_context(|| format!("rename {} to {}", from.display(), to.display())),
    }
}

#[cfg(unix)]
fn is_cross_device(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(libc::EXDEV)
}

#[cfg(not(unix))]
fn is_cross_device(_: &std::io::Error) -> bool {
    false
}

/// Copies `from` to `to`, checks size and SHA-256 of every file, then removes `from`.
fn copy_verify_remove(from: &Path, to: &Path) -> Result<()> {
    let result = copy_tree(from, to);
    if let Err(err) = result {
        let _ = if to.is_dir() { fs::remove_dir_all(to) } else { fs::remove_file(to) };
        return Err(err);
    }
    if from.is_dir() {
        fs::remove_dir_all(from)
    } else {
        fs::remove_file(from)
    }
    .with_context(|| format!("remove {}", from.display()))
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    if !from.is_dir() {
        return copy_file(from, to);
    }
    for entry in WalkDir::new(from) {
        let entry = entry?;
        let target = to.join(entry.path().strip_prefix(from)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).with_context(|| format!("create {}", target.display()))?;
        } else {
            copy_file(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub(crate) fn copy_file(from: &Path, to: &Path) -> Result<()> {
    fs::copy(from, to).with_context(|| format!("copy {} to {}", from.display(), to.display()))?;
    let (a, b) = (digest(from)?, digest(to)?);
    if a != b {
        bail!("copy of {} does not match its source", from.display());
    }
    Ok(())
}

pub(crate) fn digest(path: &Path) -> Result<(u64, Vec<u8>)> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((size, hasher.finalize().to_vec()))
}

fn history_db(env: &Env) -> PathBuf {
    env.omp_home.join("agent").join("history.db")
}

fn composer_db(env: &Env) -> PathBuf {
    env.omp_home.join("agent").join("cache").join("composer.db")
}

/// Points breadcrumbs, custom markers and the history and composer databases
/// at the moved sessions. Only relocated sessions and their cwds are touched.
fn update_indexes(items: &[Item], env: &Env, dry_run: bool) -> Result<IndexCounts> {
    let moved: Vec<&Item> = items.iter().filter(|i| i.relocated()).collect();
    if moved.is_empty() {
        return Ok(IndexCounts::default());
    }
    let agent = env.omp_home.join("agent");
    let mut counts = IndexCounts {
        breadcrumbs: rewrite_pointer_files(&agent.join("terminal-sessions"), &moved, dry_run)?,
        custom_markers: rewrite_pointer_files(&agent.join("custom-session-files"), &moved, dry_run)?,
        ..IndexCounts::default()
    };

    // A cwd shared with a session left behind still belongs to that session.
    let kept: HashSet<&str> = items.iter().filter(|i| !i.relocated()).map(|i| i.old_cwd.as_str()).collect();
    let cwds: BTreeMap<&str, &str> = moved
        .iter()
        .filter(|i| !kept.contains(i.old_cwd.as_str()))
        .map(|i| (i.old_cwd.as_str(), i.new_cwd.as_str()))
        .collect();
    let sqlite = env.sqlite.as_deref();

    let history = history_db(env);
    if history.is_file() {
        let sqlite = sqlite.ok_or_else(|| anyhow!("sqlite3 not found on PATH"))?;
        let mut stmts = Vec::new();
        for item in &moved {
            let id = quote(&item.id);
            let (old, new) = (quote(&item.old_cwd), quote(&item.new_cwd));
            for table in ["history", "session_recaps"] {
                stmts.push((
                    table,
                    format!("cwd = {new}"),
                    format!("session_id = {id} AND cwd = {old}"),
                ));
            }
        }
        for (old, new) in &cwds {
            stmts.push(("history", format!("cwd = {}", quote(new)), format!("session_id IS NULL AND cwd = {}", quote(old))));
        }
        counts.history = run_sql(sqlite, &history, &stmts, dry_run, "")?;
    }
    let composer = composer_db(env);
    if composer.is_file() {
        let sqlite = sqlite.ok_or_else(|| anyhow!("sqlite3 not found on PATH"))?;
        let stmts: Vec<_> = cwds
            .iter()
            .map(|(old, new)| ("entries", format!("project = {}", quote(new)), format!("project = {}", quote(old))))
            .collect();
        counts.composer = run_sql(sqlite, &composer, &stmts, dry_run, "OR IGNORE")?;
    }
    Ok(counts)
}

/// SQL string literal.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Runs `UPDATE <table> SET <set> WHERE <filter>` for each statement in one
/// transaction, or counts the matching rows in a dry run. Returns the total.
fn run_sql(sqlite: &Path, db: &Path, stmts: &[(&str, String, String)], dry_run: bool, conflict: &str) -> Result<usize> {
    if stmts.is_empty() {
        return Ok(0);
    }
    let mut script = String::from(".timeout 5000\n");
    if dry_run {
        for (table, _, filter) in stmts {
            script.push_str(&format!("SELECT count(*) FROM {table} WHERE {filter};\n"));
        }
    } else {
        script.push_str("BEGIN IMMEDIATE;\n");
        for (table, set, filter) in stmts {
            script.push_str(&format!("UPDATE {conflict} {table} SET {set} WHERE {filter};\nSELECT changes();\n"));
        }
        script.push_str("COMMIT;\n");
    }
    let mut cmd = Command::new(sqlite);
    cmd.arg("-batch").arg("-bail").arg("-noheader");
    if dry_run {
        cmd.arg("-readonly");
    }
    let mut child = cmd
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {}", sqlite.display()))?;
    child.stdin.take().expect("piped stdin").write_all(script.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "update {} failed: {}",
            db.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<usize>().ok())
        .sum())
}

/// Rewrites pointer files whose transcript path names a moved session, along
/// with any line holding that session's cwd. Returns how many files changed.
fn rewrite_pointer_files(dir: &Path, moved: &[&Item], dry_run: bool) -> Result<usize> {
    let Ok(entries) = fs::read_dir(dir) else { return Ok(0) };
    let mut changed = 0;
    for entry in entries {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Some(updated) = rewrite_pointer(&text, moved) else { continue };
        changed += 1;
        if !dry_run {
            write_atomic(&path, updated.as_bytes())?;
        }
    }
    Ok(changed)
}

/// The rewritten pointer text, or `None` when it names no moved session.
fn rewrite_pointer(text: &str, moved: &[&Item]) -> Option<String> {
    let session_path = |line: &str| -> Option<(String, &Item)> {
        moved.iter().find_map(|item| {
            let old = item.old_path.to_str()?;
            let new = item.new_path.to_str()?;
            if let Some(mapped) = remap(line, old, new) {
                return Some((mapped, *item));
            }
            let (old_art, new_art) = (Item::artifacts(&item.old_path), Item::artifacts(&item.new_path));
            Some((remap(line, old_art.to_str()?, new_art.to_str()?)?, *item))
        })
    };
    let owner = text.lines().find_map(|line| session_path(line).map(|(_, item)| item))?;
    let mut out = String::with_capacity(text.len());
    for part in text.split_inclusive('\n') {
        let (line, eol) = match part.strip_suffix('\n') {
            Some(line) => (line, "\n"),
            None => (part, ""),
        };
        let replaced = if let Some((mapped, _)) = session_path(line) {
            mapped
        } else if line == owner.old_cwd {
            owner.new_cwd.clone()
        } else {
            line.to_string()
        };
        out.push_str(&replaced);
        out.push_str(eol);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        root: PathBuf,
        env: Env,
        held_file: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            let root = resolve(&std::env::temp_dir()).join(format!(
                "pi-transfuse-relocate-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("omp/agent/sessions")).unwrap();
            fs::create_dir_all(root.join("work")).unwrap();
            let held_file = root.join("held.json");
            fs::write(&held_file, r#"{"held":[],"unknown":[]}"#).unwrap();
            let omp = root.join("omp-bin");
            fs::write(&omp, format!("#!/bin/sh\ncat '{}'\n", held_file.display())).unwrap();
            fs::set_permissions(&omp, fs::Permissions::from_mode(0o755)).unwrap();
            let env = Env {
                pi_root: root.join("omp/agent/sessions"),
                omp_home: root.join("omp"),
                omp: Some(omp),
                sqlite: which("sqlite3"),
                poll: Duration::from_millis(20),
            };
            Fixture { root, env, held_file }
        }

        fn dir(&self, rel: &str) -> String {
            let p = self.root.join("work").join(rel);
            fs::create_dir_all(&p).unwrap();
            p.to_str().unwrap().to_string()
        }

        fn session(&self, id: &str, cwd: &str, extra: Value) -> PathBuf {
            let path = self
                .env
                .pi_root
                .join(paths::encode_pi_bucket(cwd))
                .join(paths::pi_file_name("2026-11-01T00:00:00.000Z", id));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut header = json!({"type":"session","version":3,"id":id,"timestamp":"2026-11-01T00:00:00.000Z","cwd":cwd});
            for (k, v) in extra.as_object().unwrap() {
                header[k] = v.clone();
            }
            let body = format!(
                "{}\n{}\n{}\n{}\n",
                pi::title_slot(&format!("title {id}"), None, "2026-11-01T00:00:00.000Z"),
                header,
                r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi /x/ \u00e9"}}"#,
                r#"{"type":"message","id":"b","parentId":"a","message":{"role":"assistant","content":"ok"}}"#,
            );
            fs::write(&path, body).unwrap();
            path
        }

        fn request(&self, old: &str, new: &str) -> Request {
            Request { old: old.into(), new: new.into(), dry_run: false, wait: false, move_dir: false }
        }

        fn found(&self, id: &str) -> PathBuf {
            WalkDir::new(&self.env.pi_root)
                .min_depth(2)
                .max_depth(2)
                .into_iter()
                .filter_map(Result::ok)
                .map(|e| e.into_path())
                .find(|p| p.file_name().unwrap().to_str().unwrap() == paths::pi_file_name("2026-11-01T00:00:00.000Z", id))
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn header(path: &Path) -> Map<String, Value> {
        let bytes = fs::read(path).unwrap();
        let (s, e) = header_span(&bytes).unwrap();
        serde_json::from_slice(&bytes[s..e]).unwrap()
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| (e.path().to_path_buf(), fs::read(e.path()).unwrap()))
            .collect()
    }

    #[test]
    fn remap_respects_component_boundaries() {
        assert_eq!(remap("/a/x", "/a/x", "/b"), Some("/b".into()));
        assert_eq!(remap("/a/x/sub", "/a/x", "/b"), Some("/b/sub".into()));
        assert_eq!(remap("/a/xy", "/a/x", "/b"), None);
    }

    #[test]
    fn moves_old_and_subfolder_sessions_and_leaves_siblings() {
        let fx = Fixture::new();
        let old = fx.dir("old");
        let sub = fx.dir("old/sub");
        let other = fx.dir("old-other");
        let new = fx.dir("new");
        let a = fx.session("a", &old, json!({"additionalDirectories":[format!("{old}/lib"), "/elsewhere"]}));
        let b = fx.session("b", &sub, json!({}));
        let c = fx.session("c", &other, json!({}));
        let original = fs::read(&a).unwrap();
        // Artifact directory with a nested subagent transcript, and a lock sidecar.
        let art = Item::artifacts(&a).join("deep");
        fs::create_dir_all(&art).unwrap();
        fs::write(
            art.join("agent.jsonl"),
            format!("{}\n{{\"type\":\"message\"}}\n", json!({"type":"session","id":"s","cwd":old})),
        )
        .unwrap();
        fs::write(Item::lock(&a), "lock").unwrap();

        let report = run(&fx.request(&old, &new), &fx.env).unwrap();
        assert_eq!(report.items.len(), 2);
        assert!(report.items.iter().all(|i| i.status == Status::Moved));
        assert_eq!(report.exit_code(), 0);

        let a2 = fx.env.pi_root.join(paths::encode_pi_bucket(&new)).join(a.file_name().unwrap());
        let b2 = fx.env.pi_root.join(paths::encode_pi_bucket(&format!("{new}/sub"))).join(b.file_name().unwrap());
        assert!(a2.is_file() && b2.is_file() && !a.exists() && !b.exists());
        assert!(c.is_file());
        assert!(!a.parent().unwrap().exists(), "emptied bucket is removed");
        assert!(c.parent().unwrap().exists());

        let h = header(&a2);
        assert_eq!(h["cwd"], json!(new));
        assert_eq!(h["additionalDirectories"], json!([format!("{new}/lib"), "/elsewhere"]));
        assert_eq!(h["previousSessionFiles"], json!([a.to_str().unwrap()]));
        assert_eq!(header(&b2)["cwd"], json!(format!("{new}/sub")));
        let nested = Item::artifacts(&a2).join("deep/agent.jsonl");
        assert_eq!(header(&nested)["cwd"], json!(new));
        assert!(header(&nested).get("previousSessionFiles").is_none());
        assert!(Item::lock(&a2).is_file());

        // Only the header line differs.
        let moved = fs::read(&a2).unwrap();
        let (os, oe) = header_span(&original).unwrap();
        let (ms, me) = header_span(&moved).unwrap();
        assert_eq!(os, pi::TITLE_SLOT_BYTES);
        assert_eq!(original[..os], moved[..ms]);
        assert_eq!(original[oe..], moved[me..]);

        // A second run finds nothing and changes nothing.
        let before = snapshot(&fx.root);
        let again = run(&fx.request(&old, &new), &fx.env).unwrap();
        assert!(again.items.is_empty());
        assert_eq!(again.exit_code(), 0);
        assert_eq!(before, snapshot(&fx.root));
    }

    #[test]
    fn held_sessions_stay_put_and_wait_moves_them() {
        let fx = Fixture::new();
        let (old, new) = (fx.dir("old"), fx.dir("new"));
        let a = fx.session("a", &old, json!({}));
        fx.session("b", &old, json!({}));
        let original = fs::read(&a).unwrap();
        fs::write(&fx.held_file, r#"{"held":["a"],"unknown":[]}"#).unwrap();

        let report = run(&fx.request(&old, &new), &fx.env).unwrap();
        let status: BTreeMap<_, _> = report.items.iter().map(|i| (i.id.as_str(), i.status.clone())).collect();
        assert_eq!(status["a"], Status::Held);
        assert_eq!(status["b"], Status::Moved);
        assert_eq!(report.exit_code(), 1);
        assert_eq!(fs::read(&a).unwrap(), original);

        let held_file = fx.held_file.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            // Replace atomically: the fake omp may be reading it right now.
            write_atomic(&held_file, br#"{"held":[],"unknown":[]}"#).unwrap();
        });
        // Still held at the first check; --wait polls until released.
        let mut req = fx.request(&old, &new);
        req.wait = true;
        let report = run(&req, &fx.env).unwrap();
        release.join().unwrap();
        assert_eq!(report.items.len(), 1);
        assert_eq!(report.items[0].status, Status::Moved);
        assert!(!a.exists());
    }

    #[test]
    fn existing_destination_is_a_conflict() {
        let fx = Fixture::new();
        let (old, new) = (fx.dir("old"), fx.dir("new"));
        let a = fx.session("a", &old, json!({}));
        let b = fx.session("b", &old, json!({}));
        let dest = fx.env.pi_root.join(paths::encode_pi_bucket(&new)).join(a.file_name().unwrap());
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, "occupied").unwrap();
        let original = fs::read(&a).unwrap();

        let report = run(&fx.request(&old, &new), &fx.env).unwrap();
        let status: BTreeMap<_, _> = report.items.iter().map(|i| (i.id.as_str(), i.status.clone())).collect();
        assert_eq!(status["a"], Status::Conflict);
        assert_eq!(status["b"], Status::Moved);
        assert_eq!(fs::read(&a).unwrap(), original);
        assert_eq!(fs::read(&dest).unwrap(), b"occupied");
        assert!(!b.exists());
    }

    #[test]
    fn failure_after_transcript_rename_rolls_back() {
        let fx = Fixture::new();
        let (old, new) = (fx.dir("old"), fx.dir("new"));
        let a = fx.session("a", &old, json!({}));
        let before = snapshot(&fx.root);
        FAIL_AFTER_TRANSCRIPT.with(|f| f.set(true));
        let report = run(&fx.request(&old, &new), &fx.env).unwrap();
        FAIL_AFTER_TRANSCRIPT.with(|f| f.set(false));
        assert!(matches!(report.items[0].status, Status::Error(_)));
        assert_eq!(report.exit_code(), 1);
        assert!(a.is_file());
        assert_eq!(before, snapshot(&fx.root));
        assert!(!fx.env.pi_root.join(paths::encode_pi_bucket(&new)).exists());
    }

    #[test]
    fn indexes_follow_moved_sessions_only() {
        let fx = Fixture::new();
        let Some(sqlite) = fx.env.sqlite.clone() else { return };
        let (old, new, other) = (fx.dir("old"), fx.dir("new"), fx.dir("other"));
        let a = fx.session("a", &old, json!({}));
        let held = fx.session("h", &format!("{old}/held"), json!({}));
        let o = fx.session("o", &other, json!({}));
        fs::write(&fx.held_file, r#"{"held":["h"],"unknown":[]}"#).unwrap();

        let agent = fx.env.omp_home.join("agent");
        fs::create_dir_all(agent.join("terminal-sessions")).unwrap();
        fs::create_dir_all(agent.join("custom-session-files")).unwrap();
        fs::create_dir_all(agent.join("cache")).unwrap();
        let crumb = |p: &Path, cwd: &str| format!("{cwd}\n{}\ncwdstat 1 2\n", p.display());
        fs::write(agent.join("terminal-sessions/t1"), crumb(&a, &old)).unwrap();
        fs::write(agent.join("terminal-sessions/t2"), crumb(&held, &format!("{old}/held"))).unwrap();
        fs::write(agent.join("terminal-sessions/t3"), crumb(&o, &other)).unwrap();
        fs::write(agent.join("custom-session-files/m1"), format!("{}\n", Item::artifacts(&a).join("x.jsonl").display())).unwrap();
        fs::write(agent.join("custom-session-files/m2"), format!("{}\n", o.display())).unwrap();
        let sql = |db: &Path, script: &str| {
            let out = Command::new(&sqlite).arg(db).arg(script).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let history = agent.join("history.db");
        let composer = agent.join("cache/composer.db");
        sql(&history, &format!(
            "CREATE TABLE history (id INTEGER PRIMARY KEY, prompt TEXT, cwd TEXT, session_id TEXT);
             CREATE TABLE session_recaps (id INTEGER PRIMARY KEY, session_id TEXT, cwd TEXT);
             INSERT INTO history VALUES (1,'p','{old}','a'),(2,'q','{old}/held','h'),(3,'r','{other}','o'),(4,'s','{old}',NULL);
             INSERT INTO session_recaps VALUES (1,'a','{old}'),(2,'o','{other}');"
        ));
        sql(&composer, &format!(
            "CREATE TABLE entries (project TEXT, kind TEXT, value TEXT, PRIMARY KEY(project, kind)) WITHOUT ROWID;
             INSERT INTO entries VALUES ('{old}','k','v'),('{other}','k','v'),('{old}/held','k','v');"
        ));

        let mut dry = fx.request(&old, &new);
        dry.dry_run = true;
        let before = snapshot(&fx.root);
        let report = run(&dry, &fx.env).unwrap();
        assert_eq!(before, snapshot(&fx.root), "dry run writes nothing");
        assert_eq!(report.indexes, IndexCounts { breadcrumbs: 1, custom_markers: 1, history: 3, composer: 1 });

        let report = run(&fx.request(&old, &new), &fx.env).unwrap();
        assert_eq!(report.indexes, IndexCounts { breadcrumbs: 1, custom_markers: 1, history: 3, composer: 1 });
        let a2 = report.items.iter().find(|i| i.id == "a").unwrap().new_path.clone();
        assert_eq!(fx.found("a"), a2);
        assert_eq!(fs::read_to_string(agent.join("terminal-sessions/t1")).unwrap(), crumb(&a2, &new));
        assert_eq!(fs::read_to_string(agent.join("terminal-sessions/t2")).unwrap(), crumb(&held, &format!("{old}/held")));
        assert_eq!(fs::read_to_string(agent.join("terminal-sessions/t3")).unwrap(), crumb(&o, &other));
        assert_eq!(
            fs::read_to_string(agent.join("custom-session-files/m1")).unwrap(),
            format!("{}\n", Item::artifacts(&a2).join("x.jsonl").display())
        );
        assert_eq!(fs::read_to_string(agent.join("custom-session-files/m2")).unwrap(), format!("{}\n", o.display()));
        assert_eq!(
            sql(&history, "SELECT group_concat(cwd, ',') FROM (SELECT cwd FROM history ORDER BY id)"),
            format!("{new},{old}/held,{other},{new}")
        );
        assert_eq!(sql(&history, "SELECT group_concat(cwd, ',') FROM (SELECT cwd FROM session_recaps ORDER BY id)"), format!("{new},{other}"));
        assert_eq!(
            sql(&composer, "SELECT group_concat(project, ',') FROM (SELECT project FROM entries ORDER BY project)"),
            {
                let mut v = [new.clone(), other.clone(), format!("{old}/held")];
                v.sort();
                v.join(",")
            }
        );
    }

    #[test]
    fn move_dir_renames_and_refuses_a_non_empty_destination() {
        let fx = Fixture::new();
        let old = fx.dir("old");
        let taken = fx.dir("taken");
        fs::write(Path::new(&taken).join("f"), "x").unwrap();
        fx.session("a", &old, json!({}));
        let mut req = fx.request(&old, &taken);
        req.move_dir = true;
        assert!(run(&req, &fx.env).is_err());
        assert!(Path::new(&old).is_dir());

        let new = fx.root.join("work/renamed").to_str().unwrap().to_string();
        let mut req = fx.request(&old, &new);
        req.move_dir = true;
        let report = run(&req, &fx.env).unwrap();
        assert!(!Path::new(&old).exists() && Path::new(&new).is_dir());
        assert_eq!(report.items[0].status, Status::Moved);
        assert_eq!(header(&fx.found("a"))["cwd"], json!(new));
        // Re-running after the rename succeeds with nothing left to do.
        let again = run(&req, &fx.env).unwrap();
        assert!(again.items.is_empty());
    }

    #[test]
    fn copy_fallback_verifies_and_removes_the_source() {
        let fx = Fixture::new();
        let src = fx.root.join("src");
        fs::create_dir_all(src.join("d")).unwrap();
        fs::write(src.join("d/f"), "data").unwrap();
        copy_verify_remove(&src, &fx.root.join("dst")).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read(fx.root.join("dst/d/f")).unwrap(), b"data");
    }
}
