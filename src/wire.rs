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
use crate::askpass;
use crate::transfer::{self, Inbox, Meter, Outbox, Progress};
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
    /// pi-transfuse on the remote host; found through your login shell's PATH
    /// there, then the usual install folders, when omitted.
    #[arg(long, value_name = "PATH")]
    pub remote_bin: Option<String>,
}

impl Default for SshArgs {
    /// What the browser uses: plain `ssh`, configured through `~/.ssh/config`.
    fn default() -> SshArgs {
        SshArgs { ssh: PathBuf::from("ssh"), ssh_arg: Vec::new(), remote_bin: None }
    }
}

/// Where pi-transfuse is looked for on the remote host after the login
/// shell's PATH, `$HOME` expanded there.
const INSTALL_DIRS: [&str; 4] = ["$HOME/.local/bin", "$HOME/.cargo/bin", "/opt/homebrew/bin", "/usr/local/bin"];

/// True for text that stays itself inside double quotes in a POSIX shell and
/// inside single quotes in every login shell ssh may hand the command to.
fn plain(text: &str) -> bool {
    !text.is_empty() && !text.contains(['\'', '"', '\\', '$', '`', '!', '\n', '\r'])
}

/// A path as a double-quoted POSIX word; a leading `~/` becomes `$HOME/`.
fn remote_path(path: &str) -> Option<String> {
    let path = path.strip_prefix("~/").map_or_else(|| path.to_string(), |rest| format!("$HOME/{rest}"));
    let rest = path.strip_prefix("$HOME/").unwrap_or(&path);
    plain(rest).then(|| format!("\"{path}\""))
}

/// The command ssh runs on the remote host. ssh hands it to the user's login
/// shell non-interactively, which on most setups skips the rc files that set
/// PATH. So it asks that shell, as an interactive login shell, for its PATH,
/// then finds pi-transfuse on it or in the install folders, and runs
/// `migrate serve` with that PATH, which also lets serve find `omp`.
///
/// The script is one line of POSIX sh inside single quotes, with no single
/// quote, backslash or `!`, so bash, zsh, fish and tcsh all pass it intact.
/// The probe gets no stdin, so an rc file that reads input cannot eat the
/// protocol, and the PATH is taken from its last output line.
fn remote_command(remote_bin: Option<&str>, local_exe: Option<&str>) -> Result<String> {
    let home = crate::paths::home();
    let same_place = local_exe.and_then(|exe| match std::path::Path::new(exe).strip_prefix(&home) {
        Ok(rest) => remote_path(&format!("~/{}", rest.display())),
        Err(_) => remote_path(exe),
    });
    let mut find = String::new();
    match remote_bin {
        Some(bin) if !bin.contains('/') => {
            if !plain(bin) {
                bail!("--remote-bin {bin:?} has characters that cannot pass through every remote shell");
            }
            find.push_str(&format!("b=$(command -v \"{bin}\" 2>/dev/null); "));
        }
        Some(bin) => {
            let word = remote_path(bin).ok_or_else(|| {
                anyhow!("--remote-bin {bin:?} has characters that cannot pass through every remote shell")
            })?;
            find.push_str(&format!("b={word}; [ -x \"$b\" ] || b=; "));
        }
        None => {
            find.push_str("b=$(command -v pi-transfuse 2>/dev/null); ");
            let candidates: Vec<String> = same_place
                .into_iter()
                .chain(INSTALL_DIRS.iter().map(|dir| format!("\"{dir}/pi-transfuse\"")))
                .collect();
            find.push_str(&format!(
                "for c in {}; do [ -n \"$b\" ] && break; [ -x \"$c\" ] && b=\"$c\"; done; ",
                candidates.join(" ")
            ));
        }
    }
    let wanted = remote_bin.unwrap_or("pi-transfuse");
    let script = format!(
        "p=$(\"${{SHELL:-/bin/sh}}\" -lic \"printenv PATH\" </dev/null 2>/dev/null | tail -n 1); \
         case \"$p\" in /*) PATH=\"$p:$PATH\"; export PATH;; esac; \
         {find}\
         [ -n \"$b\" ] || {{ echo \"{wanted} was not found on $(hostname): not on the PATH of \
         ${{SHELL:-sh}} as a login shell, nor in the usual install folders; install it there \
         or name it with --remote-bin\" >&2; exit 127; }}; \
         exec \"$b\" migrate serve"
    );
    debug_assert!(!script.contains(['\'', '\\', '!', '\n']));
    Ok(format!("exec /bin/sh -c '{script}'"))
}

/// Starts ssh in a session of its own, without a controlling terminal, so it
/// asks through askpass and never draws over the browser.
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe and touches only the child.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
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

/// Where ssh's questions, ssh's messages and transfer progress go; the
/// terminal by default.
#[derive(Default)]
pub struct Launch {
    /// Answers ssh's questions through this askpass socket, with ssh detached
    /// from the terminal.
    pub askpass: Option<PathBuf>,
    /// Receives each line ssh writes to stderr.
    pub stderr: Option<Box<dyn FnMut(String) + Send>>,
    pub progress: Option<Box<Progress>>,
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
    progress: Option<Box<Progress>>,
}

impl Remote {
    pub fn connect(host: &str, ssh: &SshArgs, launch: Launch) -> Result<Remote> {
        if host.is_empty() || host.starts_with('-') {
            bail!("{host:?} is not a host");
        }
        let remote_bin = ssh.remote_bin.clone().or_else(|| std::env::var("PI_TRANSFUSE_REMOTE_BIN").ok());
        let local_exe = std::env::current_exe().ok().and_then(|p| p.to_str().map(str::to_string));
        let command = remote_command(remote_bin.as_deref(), local_exe.as_deref())?;
        let mut cmd = Command::new(&ssh.ssh);
        cmd.arg("-T").args(&ssh.ssh_arg).arg("--").arg(host).arg(command);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        cmd.stderr(if launch.stderr.is_some() { Stdio::piped() } else { Stdio::inherit() });
        if let Some(socket) = &launch.askpass {
            let me = std::env::current_exe().context("locate this program for SSH_ASKPASS")?;
            cmd.env("SSH_ASKPASS", me).env("SSH_ASKPASS_REQUIRE", "force").env(askpass::SOCKET_ENV, socket);
            detach(&mut cmd);
        }
        let mut child = cmd.spawn().with_context(|| format!("run {}", ssh.ssh.display()))?;
        if let (Some(mut sink), Some(stderr)) = (launch.stderr, child.stderr.take()) {
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    sink(line);
                }
            });
        }
        let input = BufReader::new(child.stdout.take().ok_or_else(|| anyhow!("ssh has no stdout"))?);
        let output = BufWriter::new(child.stdin.take().ok_or_else(|| anyhow!("ssh has no stdin"))?);
        let mut remote = Remote {
            host: host.to_string(),
            system: System::current(),
            pi_root: String::new(),
            child,
            input,
            output: Some(output),
            progress: launch.progress,
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
                    "no migration service on {host}{status}: ssh's own message above says why \
                     when it could not log in or find pi-transfuse there"
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
            let mut meter = Meter::new(id, size, self.progress.as_deref_mut());
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
            let mut meter = Meter::new(id, offer.size, self.progress.as_deref_mut());
            let host = self.host.clone();
            let output = self.output.as_mut().ok_or_else(|| anyhow!("the connection is closed"))?;
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
