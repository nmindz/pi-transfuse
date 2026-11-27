//! The migration protocol between two hosts over SSH. The local run starts
//! `pi-transfuse migrate serve` on the remote host through `ssh` and talks to
//! it over the session's stdin and stdout: one JSON message per line, each
//! archive as exactly the announced number of raw bytes after its message.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::archive::{self, Algo, Compression, System};
use crate::transfer::{self, Inbox, Meter, Outbox};
use crate::units::{Ctx, Status, Unit};

/// Bumped whenever a message changes shape.
const PROTOCOL: u32 = 1;
/// Lines a remote shell may print before the greeting, from noisy rc files.
const NOISE_LIMIT: usize = 200;

/// What the local run asks of the remote host.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Request {
    List,
    /// Resolve a relocation target there.
    Target { dir: String },
    /// Pack a session and send it.
    Pack { id: String, algo: Algo, level: i32 },
    /// The session arrived; drop the sent archive, and the session with `delete`.
    Release { id: String, delete: bool },
    /// Take the archive of `size` bytes that follows.
    Accept { id: String, size: u64, sha256: String, relocate: Option<String> },
    Bye,
}

/// What the remote host answers.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "camelCase")]
pub enum Reply {
    Hello { protocol: u32, system: System, pi_root: String },
    Listing { units: Vec<Unit>, held: Vec<String> },
    Target { cwd: String },
    /// `size` archive bytes follow.
    Offer { size: u64, sha256: String },
    Refused { status: Status },
    Released { freed: u64 },
    /// The session arrived but its source stays, for `reason`.
    Kept { reason: String },
    Placed { path: String, cwd: String },
    Error { message: String },
}

fn send<T: Serialize>(out: &mut impl Write, message: &T) -> Result<()> {
    serde_json::to_writer(&mut *out, message)?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

/// The next message, or `None` when the other end closed the stream.
fn recv<T: DeserializeOwned>(input: &mut impl BufRead) -> Result<Option<T>> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(&line).map(Some).with_context(|| format!("unreadable message {:?}", line.trim()))
}

/// Answers one local run on stdin and stdout until it says goodbye.
pub fn serve(ctx: &Ctx) -> Result<i32> {
    let mut input = BufReader::new(io::stdin().lock());
    let mut output = BufWriter::new(io::stdout().lock());
    let pi_root = ctx.roots.pi.display().to_string();
    send(&mut output, &Reply::Hello { protocol: PROTOCOL, system: ctx.system.clone(), pi_root })?;
    let mut outbox = Outbox::new(ctx);
    let mut inbox = Inbox::new(ctx);
    while let Some(request) = recv::<Request>(&mut input)? {
        let reply = match request {
            Request::Bye => break,
            Request::List => {
                let units = outbox.units().to_vec();
                let ids: Vec<String> = units.iter().map(|u| u.id.clone()).collect();
                let held = outbox.held(&ids).into_iter().collect();
                Reply::Listing { units, held }
            }
            Request::Target { dir } => match Inbox::target(&dir) {
                Ok(cwd) => Reply::Target { cwd },
                Err(err) => Reply::Error { message: format!("{err:#}") },
            },
            Request::Pack { id, algo, level } => {
                let threads = if algo == Algo::Zstd { archive::all_cores() } else { 0 };
                match outbox.pack(&id, Compression { algo, level, threads }) {
                    Ok(offer) => {
                        send(&mut output, &Reply::Offer { size: offer.size, sha256: offer.sha256 })?;
                        let mut file = File::open(&offer.path)?;
                        transfer::copy_exact(&mut file, &mut output, offer.size, &mut Meter::off())?;
                        output.flush()?;
                        continue;
                    }
                    Err(status) => Reply::Refused { status },
                }
            }
            Request::Release { id, delete } => match outbox.release(&id, delete) {
                Ok(freed) => Reply::Released { freed },
                Err(reason) => Reply::Kept { reason },
            },
            Request::Accept { id, size, sha256, relocate } => {
                // A stream that breaks mid-archive cannot be resynchronized.
                let received = inbox.receive(&mut input, size, &mut Meter::off())?;
                match inbox.settle(received, &id, &sha256, relocate.as_deref()) {
                    Ok(placed) => Reply::Placed { path: placed.path.display().to_string(), cwd: placed.cwd },
                    Err(status) => Reply::Refused { status },
                }
            }
        };
        send(&mut output, &reply)?;
    }
    Ok(0)
}

/// How to reach the remote host.
#[derive(Debug, Clone, Args)]
pub struct SshArgs {
    /// ssh client to run.
    #[arg(long, value_name = "PROGRAM", default_value = "ssh")]
    pub ssh: PathBuf,
    /// Extra argument for ssh, before the host; repeat for more (`--ssh-arg=-p --ssh-arg=2222`).
    #[arg(long, value_name = "ARG", allow_hyphen_values = true)]
    pub ssh_arg: Vec<String>,
    /// pi-transfuse on the remote host, when its PATH does not find it there.
    #[arg(long, value_name = "PATH", default_value = "pi-transfuse")]
    pub remote_bin: String,
}

/// Quotes one word for the remote shell; a leading `~/` stays expandable.
fn shell_word(word: &str) -> String {
    let quote = |text: &str| format!("'{}'", text.replace('\'', r"'\''"));
    match word.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
        None => quote(word),
    }
}

/// How each session migrates.
pub struct Terms {
    pub compression: Compression,
    /// Directory the sessions continue in on the receiving host, as OMP records a cwd.
    pub relocate: Option<String>,
    /// Remove each session at its source once it arrived.
    pub delete: bool,
}

/// A `migrate serve` running on the remote host.
pub struct Remote {
    pub host: String,
    pub system: System,
    pub pi_root: String,
    child: Child,
    input: BufReader<ChildStdout>,
    /// `None` once closed, which ends the remote side.
    output: Option<BufWriter<ChildStdin>>,
}

impl Remote {
    pub fn connect(host: &str, ssh: &SshArgs) -> Result<Remote> {
        if host.is_empty() || host.starts_with('-') {
            bail!("{host:?} is not a host");
        }
        let command = format!("{} migrate serve", shell_word(&ssh.remote_bin));
        let mut child = Command::new(&ssh.ssh)
            .arg("-T")
            .args(&ssh.ssh_arg)
            .arg("--")
            .arg(host)
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("run {}", ssh.ssh.display()))?;
        let input = BufReader::new(child.stdout.take().ok_or_else(|| anyhow!("ssh has no stdout"))?);
        let output = BufWriter::new(child.stdin.take().ok_or_else(|| anyhow!("ssh has no stdin"))?);
        let mut remote = Remote {
            host: host.to_string(),
            system: System::current(),
            pi_root: String::new(),
            child,
            input,
            output: Some(output),
        };
        match remote.greeting() {
            Ok((system, pi_root)) => {
                remote.system = system;
                remote.pi_root = pi_root;
                Ok(remote)
            }
            Err(err) => {
                remote.output = None;
                let _ = remote.child.kill();
                let status = remote.child.wait().map(|s| format!(" ({s})")).unwrap_or_default();
                Err(err.context(format!(
                    "no migration service on {host}{status}: is pi-transfuse installed there? \
                     --remote-bin names it when its PATH does not find it"
                )))
            }
        }
    }

    fn greeting(&mut self) -> Result<(System, String)> {
        for _ in 0..NOISE_LIMIT {
            let mut line = String::new();
            if self.input.read_line(&mut line)? == 0 {
                bail!("the connection closed");
            }
            let Ok(reply) = serde_json::from_str::<Reply>(&line) else { continue };
            return match reply {
                Reply::Hello { protocol, system, pi_root } if protocol == PROTOCOL => Ok((system, pi_root)),
                Reply::Hello { protocol, system, .. } => bail!(
                    "{} speaks migration protocol {protocol}, this one {PROTOCOL}; install the same version on both hosts",
                    system.tool
                ),
                other => bail!("unexpected greeting {other:?}"),
            };
        }
        bail!("no greeting among the first {NOISE_LIMIT} lines")
    }

    fn send(&mut self, request: &Request) -> Result<()> {
        let host = self.host.clone();
        send(self.output()?, request).with_context(|| format!("the connection to {host} broke"))
    }

    fn recv(&mut self) -> Result<Reply> {
        recv(&mut self.input)?.ok_or_else(|| anyhow!("{} closed the connection", self.host))
    }

    fn call(&mut self, request: &Request) -> Result<Reply> {
        self.send(request)?;
        self.recv()
    }

    /// Every OMP session there, and which a running omp holds.
    pub fn list(&mut self) -> Result<(Vec<Unit>, HashSet<String>)> {
        match self.call(&Request::List)? {
            Reply::Listing { units, held } => Ok((units, held.into_iter().collect())),
            other => unexpected(other),
        }
    }

    /// A relocation target there, as OMP records a cwd.
    pub fn target(&mut self, dir: &str) -> Result<String> {
        match self.call(&Request::Target { dir: dir.to_string() })? {
            Reply::Target { cwd } => Ok(cwd),
            Reply::Error { message } => bail!("on {}: {message}", self.host),
            other => unexpected(other),
        }
    }

    /// Brings session `id` here. An `Err` broke the connection; a `Status`
    /// is this session's outcome. Returns the archive size too.
    pub fn fetch(&mut self, id: &str, terms: &Terms, inbox: &mut Inbox) -> Result<(Status, u64)> {
        let pack = Request::Pack { id: id.into(), algo: terms.compression.algo, level: terms.compression.level };
        let (size, sha256) = match self.call(&pack)? {
            Reply::Offer { size, sha256 } => (size, sha256),
            Reply::Refused { status } => return Ok((status, 0)),
            other => return unexpected(other),
        };
        let received = {
            let mut meter = Meter::new(id, size);
            inbox.receive(&mut self.input, size, &mut meter)?
        };
        if let Err(status) = inbox.settle(received, id, &sha256, terms.relocate.as_deref()) {
            let _ = self.release(id, false)?;
            return Ok((status, size));
        }
        let status = match self.release(id, terms.delete)? {
            Ok(_) => Status::Done,
            Err(reason) => Status::Failed(format!("arrived, but the source stays: {reason}")),
        };
        Ok((status, size))
    }

    fn release(&mut self, id: &str, delete: bool) -> Result<Result<u64, String>> {
        match self.call(&Request::Release { id: id.into(), delete })? {
            Reply::Released { freed } => Ok(Ok(freed)),
            Reply::Kept { reason } => Ok(Err(reason)),
            other => unexpected(other),
        }
    }

    /// Sends session `id` there, then lets the local copy go: removed when
    /// `terms.delete` once the remote host has restored it.
    pub fn deliver(&mut self, id: &str, terms: &Terms, outbox: &mut Outbox) -> Result<(Status, u64)> {
        let offer = match outbox.pack(id, terms.compression) {
            Ok(offer) => offer,
            Err(status) => return Ok((status, 0)),
        };
        let accept = Request::Accept {
            id: id.into(),
            size: offer.size,
            sha256: offer.sha256.clone(),
            relocate: terms.relocate.clone(),
        };
        self.send(&accept)?;
        {
            let mut file = File::open(&offer.path).with_context(|| format!("open {}", offer.path.display()))?;
            let mut meter = Meter::new(id, offer.size);
            let host = self.host.clone();
            let output = self.output()?;
            transfer::copy_exact(&mut file, output, offer.size, &mut meter)
                .and_then(|_| output.flush().map_err(Into::into))
                .with_context(|| format!("the connection to {host} broke"))?;
        }
        match self.recv()? {
            Reply::Placed { .. } => {
                let status = match outbox.release(id, terms.delete) {
                    Ok(_) => Status::Done,
                    Err(reason) => Status::Failed(format!("arrived, but the source stays: {reason}")),
                };
                Ok((status, offer.size))
            }
            Reply::Refused { status } => {
                let _ = outbox.release(id, false);
                Ok((status, offer.size))
            }
            other => unexpected(other),
        }
    }

    fn output(&mut self) -> Result<&mut BufWriter<ChildStdin>> {
        self.output.as_mut().ok_or_else(|| anyhow!("the connection is closed"))
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        if let Some(mut output) = self.output.take() {
            // An ssh that already exited would answer the goodbye with SIGPIPE.
            if matches!(self.child.try_wait(), Ok(None)) {
                let _ = send(&mut output, &Request::Bye);
            }
            // Closing stdin ends the remote side even when it missed the goodbye.
            drop(output);
        }
        let _ = self.child.wait();
    }
}

fn unexpected<T>(reply: Reply) -> Result<T> {
    bail!("unexpected answer {reply:?}")
}
