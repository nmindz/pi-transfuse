//! Session archives: every file one session owns in a tarball, gzip or zstd
//! compressed, opening with a manifest and closing with per-file SHA-256 sums.
//! Paths inside are relative to the harness root, so
//! `tar -xf ARCHIVE -C ROOT --exclude .pi-transfuse` restores one by hand too.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::units::{self, Kind, Node, Unit};
use crate::{convert, relocate, Side};

/// Directory inside an archive that holds its manifest and sums.
pub const META_DIR: &str = ".pi-transfuse";
const FORMAT: &str = "pi-transfuse-archive";
const VERSION: u32 = 1;
/// Upper bound on a manifest or sums entry, against hostile archives.
const META_LIMIT: u64 = 64 << 20;

/// Compression algorithm of an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Algo {
    /// gzip: opens anywhere; levels 0-9, one thread.
    Gzip,
    /// Zstandard: faster and smaller; levels -7 to 22, every core.
    Zstd,
}

impl Algo {
    pub fn key(self) -> &'static str {
        match self {
            Algo::Gzip => "gzip",
            Algo::Zstd => "zstd",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Algo::Gzip => ".tar.gz",
            Algo::Zstd => ".tar.zst",
        }
    }

    /// Lowest, highest and default level.
    fn levels(self) -> (i32, i32, i32) {
        match self {
            Algo::Gzip => (0, 9, 6),
            Algo::Zstd => (-7, 22, 3),
        }
    }

    /// Recognizes a compressed stream by its magic bytes.
    fn sniff(head: &[u8]) -> Option<Algo> {
        if head.starts_with(&[0x1f, 0x8b]) {
            Some(Algo::Gzip)
        } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            Some(Algo::Zstd)
        } else {
            None
        }
    }
}

/// True for a file name this tool writes archives under.
pub fn is_archive_name(name: &str) -> bool {
    !name.starts_with('.') && [Algo::Gzip, Algo::Zstd].iter().any(|a| name.ends_with(a.extension()))
}

/// How an archive is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compression {
    pub algo: Algo,
    pub level: i32,
    /// zstd worker threads; 0 compresses on the calling thread.
    #[serde(skip)]
    pub threads: u32,
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} level {}", self.algo.key(), self.level)
    }
}

/// Compression switches of the packing commands.
#[derive(Debug, Args)]
pub struct CompressArgs {
    /// Compression algorithm.
    #[arg(long, value_enum, default_value = "gzip")]
    pub algo: Algo,
    /// Compression level; higher is smaller and slower. gzip 0-9 (default 6),
    /// zstd -7 to 22 (default 3; 19 and up are slow and very small).
    #[arg(long, allow_negative_numbers = true)]
    pub level: Option<i32>,
    /// zstd worker threads, 0 for one thread on the caller; default every core.
    #[arg(long)]
    pub threads: Option<u32>,
}

impl CompressArgs {
    pub fn resolve(&self) -> Result<Compression> {
        let (min, max, default) = self.algo.levels();
        let level = self.level.unwrap_or(default);
        if !(min..=max).contains(&level) {
            bail!("{} levels run from {min} to {max}", self.algo.key());
        }
        let threads = match self.algo {
            Algo::Gzip if self.threads.is_some_and(|t| t > 1) => {
                bail!("gzip compresses on one thread; --threads applies to --algo zstd")
            }
            Algo::Gzip => 0,
            Algo::Zstd => self.threads.unwrap_or_else(|| {
                std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
            }),
        };
        Ok(Compression { algo: self.algo, level, threads })
    }
}

/// Machine, user and tool behind a pack, push or restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct System {
    pub host: String,
    pub user: String,
    pub os: String,
    pub arch: String,
    pub tool: String,
}

impl System {
    pub fn current() -> System {
        System {
            host: hostname(),
            user: std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            tool: format!("pi-transfuse {}", env!("CARGO_PKG_VERSION")),
        }
    }

    pub fn describe(&self) -> String {
        format!("{} · {} · {}/{} · {}", self.host, self.user, self.os, self.arch, self.tool)
    }
}

#[cfg(unix)]
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for the full length passed.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(not(unix))]
fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// What an archive says about the session it holds; its first entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub harness: Side,
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub bucket: String,
    /// What the members are named after.
    pub name: String,
    /// Paths the session owns, relative to the harness root.
    pub members: Vec<String>,
    pub created: String,
    pub last_used: String,
    pub packed_at: String,
    pub files: usize,
    pub bytes: u64,
    pub subagents: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<usize>,
    pub compression: Compression,
    /// Harness root the session was packed from.
    pub root: String,
    pub packed_on: System,
}

impl Manifest {
    pub fn last_used_ms(&self) -> i64 {
        convert::from_iso(&self.last_used).unwrap_or(0)
    }

    fn manifest_entry(&self) -> String {
        format!("{META_DIR}/{}.manifest.json", self.name)
    }

    fn sums_entry(&self) -> String {
        format!("{META_DIR}/{}.sha256.json", self.name)
    }

    /// Refuses a manifest of another format, or one whose members would land
    /// anywhere but directly inside its bucket.
    fn check(&self) -> Result<()> {
        if self.format != FORMAT || self.version != VERSION {
            bail!("not a {FORMAT} version {VERSION}");
        }
        if !plain_name(&self.bucket) || !plain_name(&self.name) || self.members.is_empty() {
            bail!("manifest names no session");
        }
        for member in &self.members {
            match member.split_once('/') {
                Some((bucket, rest)) if bucket == self.bucket && plain_name(rest) => {}
                _ => bail!("manifest member {member:?} lies outside {}", self.bucket),
            }
        }
        Ok(())
    }

    /// True when `rel` is a member or lies inside one.
    fn owns(&self, rel: &str) -> bool {
        self.members.iter().any(|m| rel == m || rel.strip_prefix(m.as_str()).is_some_and(|r| r.starts_with('/')))
    }

    /// The members already present under `root`.
    pub fn occupied(&self, root: &Path) -> Vec<&str> {
        self.members
            .iter()
            .filter(|m| root.join(m).symlink_metadata().is_ok())
            .map(String::as_str)
            .collect()
    }
}

/// One path component that stays where it is put.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// A verified archive on disk.
#[derive(Debug, Clone)]
pub struct Archive {
    pub path: PathBuf,
    pub manifest: Manifest,
    pub archive_bytes: u64,
    pub archive_sha256: String,
    /// Digest of every file's path and SHA-256: equal for two archives of the
    /// same session bytes, however they were compressed.
    pub content_sha256: String,
}

/// Why a session was left alone, as opposed to a failure.
#[derive(Debug)]
pub enum Refusal {
    /// Something already occupies the destination.
    Conflict(String),
    /// The session was written to while it was being packed.
    Changed,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Conflict(what) => write!(f, "conflict: {what}"),
            Refusal::Changed => write!(f, "the session changed while it was being packed"),
        }
    }
}

impl std::error::Error for Refusal {}

/// Packs `unit` from `root` into `dest`, which must not exist yet. The archive
/// is read back and checked before it takes its name, and the session must not
/// change meanwhile. The session itself stays; removing it is the caller's call.
pub fn pack(
    root: &Path,
    unit: &Unit,
    dest: &Path,
    compression: Compression,
    system: &System,
    now_ms: i64,
) -> Result<Archive> {
    if dest.symlink_metadata().is_ok() {
        return Err(Refusal::Conflict(format!("{} exists", dest.display())).into());
    }
    let nodes = units::walk(root, &unit.members)?;
    if let Some(odd) = nodes.iter().find(|n| n.kind == Kind::Other) {
        bail!("{} is neither a file nor a directory", odd.rel);
    }
    let files: Vec<&Node> = nodes.iter().filter(|n| n.kind == Kind::File).collect();
    let manifest = Manifest {
        format: FORMAT.to_string(),
        version: VERSION,
        harness: unit.harness,
        id: unit.id.clone(),
        title: unit.title.clone(),
        cwd: unit.cwd.clone(),
        bucket: unit.bucket.clone(),
        name: unit.name.clone(),
        members: unit.members.clone(),
        created: convert::iso(unit.created_ms),
        last_used: convert::iso(units::last_used_ms(&nodes)),
        packed_at: convert::iso(now_ms),
        files: files.len(),
        bytes: files.iter().map(|n| n.size).sum(),
        subagents: unit.subagents,
        messages: unit.messages,
        compression,
        root: root.display().to_string(),
        packed_on: system.clone(),
    };
    manifest.check()?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = partial(dest);
    let result = (|| {
        write(root, &nodes, &manifest, &tmp, compression)?;
        let checked = verify(&tmp)?;
        if units::walk(root, &unit.members)? != nodes {
            return Err(Refusal::Changed.into());
        }
        if dest.symlink_metadata().is_ok() {
            return Err(Refusal::Conflict(format!("{} exists", dest.display())).into());
        }
        fs::rename(&tmp, dest).with_context(|| format!("rename {} to {}", tmp.display(), dest.display()))?;
        Ok(Archive { path: dest.to_path_buf(), ..checked })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Hidden sibling an archive is written under until it checks out.
pub fn partial(dest: &Path) -> PathBuf {
    let name = dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    dest.with_file_name(format!(".{name}.partial-{}", std::process::id()))
}

fn write(root: &Path, nodes: &[Node], manifest: &Manifest, tmp: &Path, compression: Compression) -> Result<()> {
    let file = File::create_new(tmp).with_context(|| format!("create {}", tmp.display()))?;
    let mut tar = tar::Builder::new(Sink::new(BufWriter::with_capacity(1 << 20, file), compression)?);
    let packed = convert::from_iso(&manifest.packed_at).unwrap_or(0).max(0) as u64 / 1000;
    append_bytes(&mut tar, &manifest.manifest_entry(), &serde_json::to_vec_pretty(manifest)?, packed)?;
    let mut sums = BTreeMap::new();
    for node in nodes {
        let mut header = tar::Header::new_gnu();
        header.set_mode(node.mode);
        header.set_mtime(node.mtime.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()));
        if node.kind == Kind::Dir {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            tar.append_data(&mut header, &node.rel, io::empty())?;
            continue;
        }
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(node.size);
        let path = root.join(&node.rel);
        let file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let mut reader = Hashing::new(file.take(node.size));
        tar.append_data(&mut header, &node.rel, &mut reader)
            .with_context(|| format!("archive {}", path.display()))?;
        if reader.count != node.size {
            return Err(Refusal::Changed.into());
        }
        sums.insert(node.rel.clone(), reader.finish());
    }
    append_bytes(&mut tar, &manifest.sums_entry(), &serde_json::to_vec_pretty(&sums)?, packed)?;
    let file = tar.into_inner()?.finish()?;
    file.sync_all().with_context(|| format!("sync {}", tmp.display()))?;
    Ok(())
}

fn append_bytes<W: Write>(tar: &mut tar::Builder<W>, path: &str, bytes: &[u8], mtime: u64) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_mtime(mtime);
    header.set_size(bytes.len() as u64);
    tar.append_data(&mut header, path, bytes)
}

/// The compressor an archive is written through.
enum Sink {
    Gzip(flate2::write::GzEncoder<BufWriter<File>>),
    Zstd(zstd::stream::write::Encoder<'static, BufWriter<File>>),
}

impl Sink {
    fn new(out: BufWriter<File>, compression: Compression) -> io::Result<Sink> {
        Ok(match compression.algo {
            Algo::Gzip => Sink::Gzip(flate2::write::GzEncoder::new(
                out,
                flate2::Compression::new(compression.level as u32),
            )),
            Algo::Zstd => {
                let mut encoder = zstd::stream::write::Encoder::new(out, compression.level)?;
                encoder.include_checksum(true)?;
                if compression.threads > 0 {
                    encoder.multithread(compression.threads)?;
                }
                Sink::Zstd(encoder)
            }
        })
    }

    fn finish(self) -> io::Result<File> {
        let out = match self {
            Sink::Gzip(encoder) => encoder.finish()?,
            Sink::Zstd(encoder) => encoder.finish()?,
        };
        out.into_inner().map_err(io::IntoInnerError::into_error)
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::Gzip(encoder) => encoder.write(buf),
            Sink::Zstd(encoder) => encoder.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::Gzip(encoder) => encoder.flush(),
            Sink::Zstd(encoder) => encoder.flush(),
        }
    }
}

/// A reader that hashes and counts what passes through it.
struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    count: u64,
}

impl<R> Hashing<R> {
    fn new(inner: R) -> Hashing<R> {
        Hashing { inner, hasher: Sha256::new(), count: 0 }
    }

    fn finish(self) -> String {
        hex(&self.hasher.finalize())
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.count += n as u64;
        Ok(n)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Opens an archive's tar stream, recognizing the compression from its bytes.
fn open(path: &Path) -> Result<tar::Archive<Box<dyn Read>>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let head = reader.fill_buf().with_context(|| format!("read {}", path.display()))?;
    let stream: Box<dyn Read> = match Algo::sniff(head) {
        Some(Algo::Gzip) => Box::new(flate2::bufread::GzDecoder::new(reader)),
        Some(Algo::Zstd) => Box::new(zstd::stream::read::Decoder::with_buffer(reader)?),
        None => bail!("{} is not a gzip or zstd archive", path.display()),
    };
    Ok(tar::Archive::new(stream))
}

/// Reads only an archive's manifest, its first entry.
pub fn read_manifest(path: &Path) -> Result<Manifest> {
    let mut archive = open(path)?;
    let mut entries = archive.entries()?;
    first_manifest(entries.next(), path)
}

fn first_manifest<R: Read>(entry: Option<io::Result<tar::Entry<'_, R>>>, path: &Path) -> Result<Manifest> {
    let entry = entry.ok_or_else(|| anyhow!("{} is empty", path.display()))??;
    let rel = entry_rel(&entry)?;
    if !(rel.starts_with(&format!("{META_DIR}/")) && rel.ends_with(".manifest.json")) {
        bail!("{} does not open with a pi-transfuse manifest", path.display());
    }
    let mut text = Vec::new();
    entry.take(META_LIMIT).read_to_end(&mut text)?;
    let manifest: Manifest =
        serde_json::from_slice(&text).with_context(|| format!("parse the manifest of {}", path.display()))?;
    manifest.check().with_context(|| path.display().to_string())?;
    if rel != manifest.manifest_entry() {
        bail!("{}: manifest entry {rel} does not match its session", path.display());
    }
    Ok(manifest)
}

/// An entry's path as a `/`-joined relative path; anything else is refused.
fn entry_rel<R: Read>(entry: &tar::Entry<'_, R>) -> Result<String> {
    let path = entry.path()?;
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => {
                parts.push(name.to_str().ok_or_else(|| anyhow!("{} is not UTF-8", path.display()))?)
            }
            Component::CurDir => {}
            _ => bail!("unsafe path in archive: {}", path.display()),
        }
    }
    if parts.is_empty() {
        bail!("empty path in archive");
    }
    Ok(parts.join("/"))
}

/// What a member entry is, refusing entries outside the session or of a kind
/// an archive never holds.
fn member_kind<R: Read>(manifest: &Manifest, rel: &str, entry: &tar::Entry<'_, R>) -> Result<Kind> {
    if !manifest.owns(rel) {
        bail!("{rel} lies outside the session's members");
    }
    match entry.header().entry_type() {
        tar::EntryType::Regular => Ok(Kind::File),
        tar::EntryType::Directory => Ok(Kind::Dir),
        other => bail!("{rel}: unsupported entry type {other:?}"),
    }
}

fn read_sums<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<BTreeMap<String, String>> {
    Ok(serde_json::from_reader(entry.take(META_LIMIT))?)
}

/// The sums when they match exactly the files seen.
fn check_sums(
    path: &Path,
    seen: BTreeMap<String, String>,
    sums: Option<BTreeMap<String, String>>,
) -> Result<BTreeMap<String, String>> {
    let sums = sums.ok_or_else(|| anyhow!("{} is truncated: its checksums are missing", path.display()))?;
    if let Some((rel, _)) = sums.iter().find(|(rel, sum)| seen.get(*rel) != Some(*sum)) {
        bail!("{}: {rel} is missing or does not match its checksum", path.display());
    }
    if let Some(rel) = seen.keys().find(|rel| !sums.contains_key(*rel)) {
        bail!("{}: {rel} has no checksum", path.display());
    }
    Ok(sums)
}

fn content_digest(sums: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (rel, sum) in sums {
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update(sum.as_bytes());
        hasher.update([b'\n']);
    }
    hex(&hasher.finalize())
}

/// Reads to the end of the stream, which is where gzip and zstd check their
/// own integrity.
fn drain(mut stream: Box<dyn Read>) -> Result<()> {
    io::copy(&mut stream, &mut io::sink()).context("archive stream is corrupt")?;
    Ok(())
}

/// Reads a whole archive and checks every file against its sums.
pub fn verify(path: &Path) -> Result<Archive> {
    let mut archive = open(path)?;
    let (manifest, sums) = {
        let mut entries = archive.entries()?;
        let manifest = first_manifest(entries.next(), path)?;
        let mut seen = BTreeMap::new();
        let mut sums = None;
        for entry in entries {
            let mut entry = entry?;
            let rel = entry_rel(&entry)?;
            if sums.is_some() {
                bail!("{}: entries follow the checksums", path.display());
            }
            if rel == manifest.sums_entry() {
                sums = Some(read_sums(&mut entry)?);
                continue;
            }
            if member_kind(&manifest, &rel, &entry)? == Kind::File {
                let mut hashing = Hashing::new(&mut entry);
                io::copy(&mut hashing, &mut io::sink())?;
                if seen.insert(rel.clone(), hashing.finish()).is_some() {
                    bail!("{}: {rel} appears twice", path.display());
                }
            }
        }
        (manifest, check_sums(path, seen, sums)?)
    };
    drain(archive.into_inner())?;
    let (archive_bytes, sha) = relocate::digest(path)?;
    Ok(Archive {
        path: path.to_path_buf(),
        content_sha256: content_digest(&sums),
        manifest,
        archive_bytes,
        archive_sha256: hex(&sha),
    })
}

static STAGING: AtomicUsize = AtomicUsize::new(0);

/// Restores an archive's session under `root`, each member exactly where it
/// was packed from. Every file is checked against the sums before any member
/// takes its place, and nothing present is overwritten.
pub fn extract(path: &Path, root: &Path) -> Result<Manifest> {
    let manifest = read_manifest(path)?;
    let taken = manifest.occupied(root);
    if !taken.is_empty() {
        return Err(Refusal::Conflict(format!("{} already exists", taken.join(", "))).into());
    }
    let bucket = root.join(&manifest.bucket);
    let made_bucket = !bucket.exists();
    fs::create_dir_all(&bucket).with_context(|| format!("create {}", bucket.display()))?;
    if made_bucket {
        // A new bucket is as private as the harness root holding it.
        if let Ok(meta) = fs::metadata(root) {
            set_mode(&bucket, units::mode(&meta));
        }
    }
    let staging = bucket.join(format!(
        ".pi-transfuse-restore-{}-{}",
        std::process::id(),
        STAGING.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir(&staging).with_context(|| format!("create {}", staging.display()))?;
    let result = unpack(path, &manifest, &staging).and_then(|dirs| {
        promote(&manifest, &staging, &bucket)?;
        settle_dirs(root, &dirs);
        Ok(())
    });
    let _ = fs::remove_dir_all(&staging);
    if result.is_err() && made_bucket {
        let _ = fs::remove_dir(&bucket);
    }
    result.map(|()| manifest)
}

/// A directory's recorded mode and time, applied once its contents are in.
struct DirMeta {
    rel: String,
    mode: u32,
    mtime: u64,
}

/// Writes every member into `staging` and checks it against the sums.
fn unpack(path: &Path, manifest: &Manifest, staging: &Path) -> Result<Vec<DirMeta>> {
    let prefix = format!("{}/", manifest.bucket);
    let mut archive = open(path)?;
    let mut dirs = Vec::new();
    {
        let mut entries = archive.entries()?;
        if first_manifest(entries.next(), path)? != *manifest {
            bail!("{} changed while it was being read", path.display());
        }
        let mut seen = BTreeMap::new();
        let mut sums = None;
        for entry in entries {
            let mut entry = entry?;
            let rel = entry_rel(&entry)?;
            if sums.is_some() {
                bail!("{}: entries follow the checksums", path.display());
            }
            if rel == manifest.sums_entry() {
                sums = Some(read_sums(&mut entry)?);
                continue;
            }
            let kind = member_kind(manifest, &rel, &entry)?;
            let local = staging.join(rel.strip_prefix(&prefix).unwrap_or(&rel));
            let mode = entry.header().mode()? & 0o7777;
            let mtime = entry.header().mtime()?;
            if kind == Kind::Dir {
                fs::create_dir_all(&local).with_context(|| format!("create {}", local.display()))?;
                dirs.push(DirMeta { rel, mode, mtime });
                continue;
            }
            if let Some(parent) = local.parent() {
                fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
            }
            let mut out = File::create_new(&local).with_context(|| format!("create {}", local.display()))?;
            let mut hashing = Hashing::new(&mut entry);
            io::copy(&mut hashing, &mut out).with_context(|| format!("write {}", local.display()))?;
            out.set_modified(UNIX_EPOCH + Duration::from_secs(mtime))?;
            out.sync_all()?;
            drop(out);
            set_mode(&local, mode);
            if seen.insert(rel.clone(), hashing.finish()).is_some() {
                bail!("{}: {rel} appears twice", path.display());
            }
        }
        check_sums(path, seen, sums)?;
    }
    drain(archive.into_inner())?;
    Ok(dirs)
}

/// Moves each staged member into its bucket, undoing the moves on failure.
fn promote(manifest: &Manifest, staging: &Path, bucket: &Path) -> Result<()> {
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = (|| {
        for member in &manifest.members {
            let name = member.rsplit('/').next().unwrap_or(member);
            let from = staging.join(name);
            if from.symlink_metadata().is_err() {
                continue;
            }
            let to = bucket.join(name);
            if to.symlink_metadata().is_ok() {
                return Err(Refusal::Conflict(format!("{member} already exists")).into());
            }
            fs::rename(&from, &to).with_context(|| format!("move {} into place", to.display()))?;
            moved.push((from, to));
        }
        File::open(bucket).and_then(|dir| dir.sync_all()).with_context(|| format!("sync {}", bucket.display()))
    })();
    if result.is_err() {
        for (from, to) in moved.into_iter().rev() {
            let _ = fs::rename(&to, &from);
        }
    }
    result
}

/// Applies directory modes and times, deepest first, now that nothing more
/// is written inside them.
fn settle_dirs(root: &Path, dirs: &[DirMeta]) {
    for dir in dirs.iter().rev() {
        let path = root.join(&dir.rel);
        if let Ok(handle) = File::open(&path) {
            let _ = handle.set_modified(UNIX_EPOCH + Duration::from_secs(dir.mtime));
        }
        set_mode(&path, dir.mode);
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777));
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) {}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::tests::{tree, Scratch};
    use std::time::SystemTime;

    fn secs(time: SystemTime) -> u64 {
        time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
    }

    fn gzip() -> Compression {
        Compression { algo: Algo::Gzip, level: 6, threads: 0 }
    }

    fn zstd() -> Compression {
        Compression { algo: Algo::Zstd, level: 3, threads: 2 }
    }

    fn pack_first(scratch: &Scratch, compression: Compression) -> (Unit, Archive) {
        let ctx = scratch.ctx();
        let unit = units::discover(&scratch.roots, &[Side::Pi]).remove(0);
        let dest = scratch.dir.join("out").join(format!("{}{}", unit.name, compression.algo.extension()));
        let archive = pack(&scratch.roots.pi, &unit, &dest, compression, &ctx.system, ctx.now_ms).unwrap();
        (unit, archive)
    }

    #[test]
    fn packed_session_restores_byte_identical_with_modes_and_times() {
        for compression in [gzip(), zstd()] {
            let scratch = Scratch::new("archive-roundtrip");
            scratch.pi_session("s1", "/work/proj");
            let before = tree(&scratch.roots.pi);
            let (unit, archive) = pack_first(&scratch, compression);
            assert_eq!(archive.manifest.files, 4);
            assert_eq!(archive.manifest.compression.algo, compression.algo);
            assert_eq!(read_manifest(&archive.path).unwrap(), archive.manifest);

            units::remove_members(&scratch.roots.pi, &unit.members).unwrap();
            let restored = extract(&archive.path, &scratch.roots.pi).unwrap();
            assert_eq!(restored.id, "s1");
            assert_eq!(tree(&scratch.roots.pi), before, "{compression}");
        }
    }

    #[test]
    fn identical_sessions_share_a_content_digest_across_algorithms() {
        let scratch = Scratch::new("archive-digest");
        scratch.pi_session("s1", "/work/proj");
        let (_, a) = pack_first(&scratch, gzip());
        let (_, b) = pack_first(&scratch, zstd());
        assert_eq!(a.content_sha256, b.content_sha256);
        assert_ne!(a.archive_sha256, b.archive_sha256);
    }

    #[test]
    fn restoring_over_an_existing_session_is_a_conflict_and_writes_nothing() {
        let scratch = Scratch::new("archive-conflict");
        scratch.pi_session("s1", "/work/proj");
        let (_, archive) = pack_first(&scratch, gzip());
        let before = tree(&scratch.roots.pi);
        let err = extract(&archive.path, &scratch.roots.pi).unwrap_err();
        assert!(matches!(err.downcast_ref::<Refusal>(), Some(Refusal::Conflict(_))));
        assert_eq!(tree(&scratch.roots.pi), before);
    }

    #[test]
    fn a_damaged_archive_restores_nothing() {
        let scratch = Scratch::new("archive-damaged");
        scratch.pi_session("s1", "/work/proj");
        let (unit, archive) = pack_first(&scratch, gzip());
        units::remove_members(&scratch.roots.pi, &unit.members).unwrap();
        let bucket = scratch.roots.pi.join(&unit.bucket);
        let empty = tree(&scratch.roots.pi);

        // Cut the stream short: the sums entry never arrives.
        let bytes = fs::read(&archive.path).unwrap();
        let cut = scratch.dir.join("cut.tar.gz");
        fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
        assert!(extract(&cut, &scratch.roots.pi).is_err());
        assert!(verify(&cut).is_err());
        assert_eq!(tree(&scratch.roots.pi), empty);
        assert!(fs::read_dir(&bucket).map_or(true, |mut d| d.next().is_none()), "no staging left behind");
    }

    #[test]
    fn a_file_altered_inside_the_archive_fails_its_checksum() {
        let scratch = Scratch::new("archive-tamper");
        scratch.pi_session("s1", "/work/proj");
        let (unit, archive) = pack_first(&scratch, gzip());
        units::remove_members(&scratch.roots.pi, &unit.members).unwrap();

        // Rewrite the archive with one member's bytes changed but the sums kept.
        let forged = scratch.dir.join("forged.tar.gz");
        let mut input = open(&archive.path).unwrap();
        let mut out = tar::Builder::new(Sink::new(BufWriter::new(File::create(&forged).unwrap()), gzip()).unwrap());
        for entry in input.entries().unwrap() {
            let mut entry = entry.unwrap();
            let mut header = entry.header().clone();
            let path = entry.path().unwrap().into_owned();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            if path.to_string_lossy().ends_with("notes.txt") {
                data[0] ^= 0xff;
            }
            out.append_data(&mut header, &path, data.as_slice()).unwrap();
        }
        out.into_inner().unwrap().finish().unwrap();

        let err = extract(&forged, &scratch.roots.pi).unwrap_err();
        assert!(format!("{err:#}").contains("notes.txt"), "{err:#}");
        assert!(unit.members.iter().all(|m| !scratch.roots.pi.join(m).exists()));
    }

    #[test]
    fn entries_outside_the_members_are_refused() {
        let scratch = Scratch::new("archive-escape");
        scratch.pi_session("s1", "/work/proj");
        let (unit, archive) = pack_first(&scratch, gzip());
        units::remove_members(&scratch.roots.pi, &unit.members).unwrap();

        let evil = scratch.dir.join("evil.tar.gz");
        let mut input = open(&archive.path).unwrap();
        let mut out = tar::Builder::new(Sink::new(BufWriter::new(File::create(&evil).unwrap()), gzip()).unwrap());
        let mut entries = input.entries().unwrap();
        let mut first = entries.next().unwrap().unwrap();
        let mut header = first.header().clone();
        let path = first.path().unwrap().into_owned();
        let mut data = Vec::new();
        first.read_to_end(&mut data).unwrap();
        out.append_data(&mut header, &path, data.as_slice()).unwrap();
        // The tar crate refuses to write `..`, so the name goes in raw.
        let mut header = tar::Header::new_gnu();
        let name = format!("{}/../escaped", unit.bucket);
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(4);
        header.set_mode(0o644);
        header.set_cksum();
        out.append(&header, &b"evil"[..]).unwrap();
        out.into_inner().unwrap().finish().unwrap();

        assert!(extract(&evil, &scratch.roots.pi).is_err());
        assert!(!scratch.roots.pi.join("escaped").exists());
    }

    #[test]
    fn level_ranges_follow_the_algorithm() {
        let args = |algo, level, threads| CompressArgs { algo, level, threads };
        assert_eq!(args(Algo::Gzip, None, None).resolve().unwrap().level, 6);
        assert!(args(Algo::Gzip, Some(10), None).resolve().is_err());
        assert_eq!(args(Algo::Zstd, Some(-7), Some(0)).resolve().unwrap().threads, 0);
        assert!(args(Algo::Zstd, Some(23), None).resolve().is_err());
        assert!(args(Algo::Gzip, None, Some(4)).resolve().is_err());
    }

    #[test]
    fn restored_times_survive_at_second_precision() {
        let scratch = Scratch::new("archive-times");
        let transcript = scratch.pi_session("s1", "/work/proj");
        let old = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        File::options().write(true).open(&transcript).unwrap().set_modified(old).unwrap();
        let (unit, archive) = pack_first(&scratch, gzip());
        units::remove_members(&scratch.roots.pi, &unit.members).unwrap();
        extract(&archive.path, &scratch.roots.pi).unwrap();
        assert_eq!(secs(fs::metadata(&transcript).unwrap().modified().unwrap()), 1_700_000_000);
    }
}
