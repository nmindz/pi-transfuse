//! The two ends of a session migration. The host holding a session packs it
//! and lets it go only once the other host has it; the receiving host checks
//! every byte, restores the session and puts it in the bucket of its cwd.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::archive::{self, Compression};
use crate::units::{self, Ctx, Node, Status, Unit};
use crate::{human_size, paths, relocate, Side};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn next() -> usize {
    NEXT.fetch_add(1, Ordering::SeqCst)
}

/// A private working directory beside the Pi root, removed on drop: on the
/// sessions' filesystem, and outside every directory OMP lists.
struct Staging {
    dir: PathBuf,
}

impl Staging {
    fn new(root: &Path) -> Result<Staging> {
        let name = format!(".pi-transfuse-migrate-{}-{}", std::process::id(), next());
        let dir = root.parent().map_or_else(|| root.join(&name), |parent| parent.join(&name));
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Staging { dir })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The staging directory, made on first use.
fn staging_dir(slot: &mut Option<Staging>, root: &Path) -> Result<PathBuf> {
    if let Some(staging) = slot {
        return Ok(staging.dir.clone());
    }
    let staging = Staging::new(root)?;
    let dir = staging.dir.clone();
    *slot = Some(staging);
    Ok(dir)
}

fn failed(err: anyhow::Error) -> Status {
    Status::Failed(format!("{err:#}"))
}

/// An archive packed for sending.
#[derive(Debug, Clone)]
pub struct Offer {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

/// A session packed and sent, kept until the other host has it.
struct Sent {
    unit: Unit,
    nodes: Vec<Node>,
    path: PathBuf,
}

/// The OMP sessions this host gives away.
pub struct Outbox<'a> {
    ctx: &'a Ctx,
    units: Option<Vec<Unit>>,
    staging: Option<Staging>,
    sent: HashMap<String, Sent>,
    noted: bool,
}

impl<'a> Outbox<'a> {
    pub fn new(ctx: &'a Ctx) -> Outbox<'a> {
        Outbox { ctx, units: None, staging: None, sent: HashMap::new(), noted: false }
    }

    /// Every OMP session here, discovered on first use.
    pub fn units(&mut self) -> &[Unit] {
        let ctx = self.ctx;
        self.units.get_or_insert_with(|| units::discover(&ctx.roots, &[Side::Pi]))
    }

    /// Which of `ids` a running omp holds.
    pub fn held(&mut self, ids: &[String]) -> HashSet<String> {
        let ctx = self.ctx;
        let wanted: HashSet<&String> = ids.iter().collect();
        let picked: Vec<&Unit> = self.units().iter().filter(|u| wanted.contains(&u.id)).collect();
        units::held_pi(ctx, &picked)
    }

    /// Whether a running omp holds `id`. Without omp nothing counts as held,
    /// and the unchanged-files check is the guard, as for `compact pack`.
    fn open(&mut self, id: &str) -> Result<bool, String> {
        let Some(omp) = self.ctx.omp.as_deref() else {
            if !self.noted {
                eprintln!("note: omp is not on PATH; open Pi sessions cannot be detected");
                self.noted = true;
            }
            return Ok(false);
        };
        relocate::lease(omp, &[id.to_string()])
            .map(|held| held.contains(id))
            .map_err(|err| format!("cannot ask omp whether it is open: {err:#}"))
    }

    /// Packs session `id`, refusing one a running omp holds or one that
    /// changes while it is packed.
    pub fn pack(&mut self, id: &str, compression: Compression) -> Result<Offer, Status> {
        let unit = self
            .units()
            .iter()
            .find(|u| u.id == id)
            .cloned()
            .ok_or_else(|| Status::Failed(format!("no OMP session {id} here")))?;
        if self.open(id).map_err(Status::Failed)? {
            return Err(Status::Held);
        }
        let root = &self.ctx.roots.pi;
        let nodes = units::walk(root, &unit.members).map_err(failed)?;
        let dir = staging_dir(&mut self.staging, root).map_err(failed)?;
        let dest = dir.join(format!("{}-{}{}", next(), unit.name, compression.algo.extension()));
        let packed = archive::pack_nodes(root, &unit, &nodes, &dest, compression, &self.ctx.system, self.ctx.now_ms)
            .map_err(Status::from_error)?;
        let offer = Offer { path: dest, size: packed.archive_bytes, sha256: packed.archive_sha256 };
        self.sent.insert(unit.id.clone(), Sent { unit, nodes, path: offer.path.clone() });
        Ok(offer)
    }

    /// Drops what was packed for `id`. With `delete`, also removes the
    /// session, unless a running omp opened it or it changed since it was
    /// packed. Returns the bytes freed, or why the session stays.
    pub fn release(&mut self, id: &str, delete: bool) -> Result<u64, String> {
        let sent = self.sent.remove(id).ok_or_else(|| format!("nothing was sent for {id}"))?;
        let _ = fs::remove_file(&sent.path);
        if !delete {
            return Ok(0);
        }
        if self.open(id)? {
            return Err("a running omp opened it meanwhile".into());
        }
        let root = &self.ctx.roots.pi;
        match units::walk(root, &sent.unit.members) {
            Ok(now) if now == sent.nodes => {}
            Ok(_) => return Err("it changed after it was sent".into()),
            Err(err) => return Err(format!("{err:#}")),
        }
        units::remove_members(root, &sent.unit.members).map_err(|err| format!("{err:#}"))?;
        // A bucket left empty goes too; one still holding anything stays.
        let _ = fs::remove_dir(root.join(&sent.unit.bucket));
        if let Some(units) = self.units.as_mut() {
            units.retain(|u| u.id != id);
        }
        Ok(sent.unit.bytes)
    }
}

/// An archive received into staging, with the SHA-256 of what arrived.
pub struct Received {
    path: PathBuf,
    sha256: String,
}

/// Where a received session landed.
#[derive(Debug, Clone)]
pub struct Placed {
    pub path: PathBuf,
    pub cwd: String,
}

/// Where OMP sessions arrive on this host.
pub struct Inbox<'a> {
    ctx: &'a Ctx,
    present: Option<HashSet<String>>,
    staging: Option<Staging>,
}

impl<'a> Inbox<'a> {
    pub fn new(ctx: &'a Ctx) -> Inbox<'a> {
        Inbox { ctx, present: None, staging: None }
    }

    /// Ids of the OMP sessions already here.
    pub fn present(&mut self) -> &HashSet<String> {
        let ctx = self.ctx;
        self.present.get_or_insert_with(|| {
            units::discover(&ctx.roots, &[Side::Pi]).into_iter().map(|u| u.id).collect()
        })
    }

    /// A relocation target on this host, as OMP records a cwd. `~` is this
    /// host's home; the directory must exist.
    pub fn target(dir: &str) -> Result<String> {
        let path = match dir.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                paths::home().join(rest.trim_start_matches('/'))
            }
            _ => PathBuf::from(dir),
        };
        relocate::existing_dir(&path)
    }

    /// Reads exactly `size` bytes of an archive into staging. An error means
    /// the stream broke or this host cannot hold the bytes; either way the
    /// stream is out of step and the connection ends.
    pub fn receive(&mut self, from: &mut dyn Read, size: u64, meter: &mut Meter) -> Result<Received> {
        let dir = staging_dir(&mut self.staging, &self.ctx.roots.pi)?;
        let path = dir.join(format!(".incoming-{}", next()));
        let result = File::create(&path)
            .with_context(|| format!("create {}", path.display()))
            .and_then(|file| {
                let mut out = BufWriter::new(file);
                let sha256 = copy_exact(from, &mut out, size, meter)?;
                out.flush()?;
                Ok(sha256)
            });
        match result {
            Ok(sha256) => Ok(Received { path, sha256 }),
            Err(err) => {
                let _ = fs::remove_file(&path);
                Err(err)
            }
        }
    }

    /// Checks a received archive against the SHA-256 the sender announced and
    /// against its own per-file sums, then restores session `id` into the
    /// bucket of its cwd, or of `relocate` when given.
    pub fn settle(&mut self, received: Received, id: &str, sha256: &str, relocate: Option<&str>) -> Result<Placed, Status> {
        let result = self.place(&received, id, sha256, relocate);
        let _ = fs::remove_file(&received.path);
        result
    }

    fn place(&mut self, received: &Received, id: &str, sha256: &str, relocate: Option<&str>) -> Result<Placed, Status> {
        if received.sha256 != sha256 {
            return Err(Status::Failed(format!(
                "the archive arrived damaged: SHA-256 {} where the sender has {sha256}",
                received.sha256
            )));
        }
        let checked = archive::verify(&received.path).map_err(failed)?;
        let m = &checked.manifest;
        if m.harness != Side::Pi || m.id != id {
            return Err(Status::Failed(format!("the archive holds {} session {}, not {id}", m.harness.key(), m.id)));
        }
        if self.present().contains(id) {
            return Err(Status::Conflict(format!("session {id} is already here")));
        }
        let cwd = relocate.unwrap_or(&m.cwd).to_string();
        let file = format!("{}.jsonl", m.name);
        let root = &self.ctx.roots.pi;
        let to = root.join(paths::encode_pi_bucket(&cwd)).join(&file);
        let dir = staging_dir(&mut self.staging, root).map_err(failed)?;
        let unpacked = dir.join(format!("unpacked-{}", next()));
        let remap = relocate.filter(|new| *new != m.cwd).map(|new| (m.cwd.as_str(), new));
        let placed = archive::extract(&received.path, &unpacked)
            .and_then(|_| relocate::place(&unpacked.join(&m.bucket).join(&file), &to, remap));
        let _ = fs::remove_dir_all(&unpacked);
        placed.map_err(Status::from_error)?;
        if let Some(present) = self.present.as_mut() {
            present.insert(id.to_string());
        }
        Ok(Placed { path: to, cwd })
    }
}

/// Copies exactly `size` bytes and returns their SHA-256; a stream that ends
/// early is an error.
pub fn copy_exact(from: &mut dyn Read, to: &mut dyn Write, size: u64, meter: &mut Meter) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut left = size;
    while left > 0 {
        let want = usize::try_from(left).map_or(buf.len(), |left| left.min(buf.len()));
        let n = match from.read(&mut buf[..want]) {
            Ok(0) => bail!("the stream ended {left} bytes short"),
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.into()),
        };
        hasher.update(&buf[..n]);
        to.write_all(&buf[..n])?;
        left -= n as u64;
        meter.add(n as u64);
    }
    Ok(archive::hex(&hasher.finalize()))
}

/// Receives transfer progress: label, bytes done, bytes total.
pub type Progress = dyn FnMut(&str, u64, u64) + Send;

/// Transfer progress at most ten times a second: to `sink` when given, else
/// redrawn in place on stderr when that is a terminal.
pub struct Meter<'a> {
    label: String,
    total: u64,
    done: u64,
    start: Instant,
    drawn: Option<Instant>,
    on: bool,
    sink: Option<&'a mut Progress>,
}

impl<'a> Meter<'a> {
    pub fn new(label: &str, total: u64, sink: Option<&'a mut Progress>) -> Meter<'a> {
        let on = sink.is_some() || io::stderr().is_terminal();
        Meter { label: crate::clip(label, 40), total, done: 0, start: Instant::now(), drawn: None, on, sink }
    }

    pub fn off() -> Meter<'static> {
        Meter { label: String::new(), total: 0, done: 0, start: Instant::now(), drawn: None, on: false, sink: None }
    }

    fn add(&mut self, n: u64) {
        self.done += n;
        if !self.on {
            return;
        }
        let now = Instant::now();
        if self.drawn.is_some_and(|at| now - at < Duration::from_millis(100)) && self.done < self.total {
            return;
        }
        self.drawn = Some(now);
        if let Some(sink) = self.sink.as_mut() {
            sink(&self.label, self.done, self.total);
            return;
        }
        let secs = (now - self.start).as_secs_f64().max(0.001);
        let rate = (self.done as f64 / secs) as u64;
        let percent = self.done.saturating_mul(100).checked_div(self.total).unwrap_or(100);
        eprint!(
            "\r\x1b[2K  {}  {percent:>3}%  {} / {}  {}/s",
            self.label,
            human_size(self.done),
            human_size(self.total),
            human_size(rate)
        );
    }
}

impl Drop for Meter<'_> {
    fn drop(&mut self) {
        if self.on && self.sink.is_none() && self.drawn.is_some() {
            eprint!("\r\x1b[2K");
        }
    }
}
