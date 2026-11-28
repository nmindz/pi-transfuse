//! `migrate` end to end: two OMP homes on this machine stand for two hosts,
//! and a fake `ssh` runs the remote side with the other home.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{json, Value};
use walkdir::WalkDir;

const BIN: &str = env!("CARGO_BIN_EXE_pi-transfuse");
const STAMP: &str = "2026-01-01T00-00-00-000Z";
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Hosts {
    dir: PathBuf,
}

impl Hosts {
    fn new() -> Hosts {
        let base = std::env::temp_dir().join(format!("pi-transfuse-migrate-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst)));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let dir = fs::canonicalize(&base).unwrap();
        for sub in ["local/agent/sessions", "remote/agent/sessions", "home", "tmp", "bin", "work/proj/sub", "work/new"] {
            fs::create_dir_all(dir.join(sub)).unwrap();
        }
        fs::write(dir.join("held.json"), r#"{"held":[],"unknown":[]}"#).unwrap();
        script(&dir.join("bin/omp"), "#!/bin/sh\ncat \"$HELD_FILE\"\n");
        script(
            &dir.join("bin/ssh"),
            "#!/bin/sh\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift 2\nOMP_HOME=\"$REMOTE_OMP_HOME\" exec sh -c \"$1\"\n",
        );
        Hosts { dir }
    }

    fn sessions(&self, host: &str) -> PathBuf {
        self.dir.join(host).join("agent/sessions")
    }

    fn work(&self, rel: &str) -> String {
        self.dir.join("work").join(rel).to_str().unwrap().to_string()
    }

    /// OMP's bucket for a cwd outside home and the temp root.
    fn bucket(cwd: &str) -> String {
        format!("--{}--", cwd.trim_start_matches('/').replace('/', "-"))
    }

    fn transcript(&self, host: &str, id: &str, cwd: &str) -> PathBuf {
        self.sessions(host).join(Self::bucket(cwd)).join(format!("{STAMP}_{id}.jsonl"))
    }

    /// A session with a subagent, a plain artifact file and a lock sidecar.
    fn session(&self, host: &str, id: &str, cwd: &str) -> PathBuf {
        let path = self.transcript(host, id, cwd);
        let stem = path.with_extension("");
        fs::create_dir_all(stem.join("agents")).unwrap();
        let header = json!({
            "type": "session", "version": 3, "id": id, "timestamp": "2026-01-01T00:00:00.000Z",
            "cwd": cwd, "additionalDirectories": [format!("{cwd}/sub"), "/elsewhere"],
        });
        let title = json!({"type": "title", "title": format!("title {id}")});
        let body = format!(
            "{title}\n{header}\n{}\n{}\n",
            r#"{"type":"message","id":"a","parentId":null,"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"message","id":"b","parentId":"a","message":{"role":"assistant","content":"ok"}}"#,
        );
        fs::write(&path, body).unwrap();
        let sub = json!({"type": "session", "version": 3, "id": format!("{id}-sub"), "timestamp": "2026-01-01T00:00:00.000Z", "cwd": cwd});
        fs::write(stem.join("agents/sub.jsonl"), format!("{sub}\n")).unwrap();
        fs::write(stem.join("notes.bin"), vec![9u8; 50_000]).unwrap();
        let lock = path.with_file_name(format!(".{STAMP}_{id}.jsonl.lock.os"));
        fs::write(lock, "").unwrap();
        path
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self
            .command(args, "ssh")
            .args(["--remote-bin", BIN])
            .output()
            .unwrap();
        eprintln!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        output
    }

    /// This program in the local host's environment, reaching the remote one
    /// through the fake ssh `ssh` in `bin`.
    fn command(&self, args: &[&str], ssh: &str) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .args(["--ssh", self.dir.join("bin").join(ssh).to_str().unwrap()])
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.dir.join("bin").display()))
            .env("HOME", self.dir.join("home"))
            .env("TMPDIR", self.dir.join("tmp"))
            .env("OMP_HOME", self.dir.join("local"))
            .env("REMOTE_OMP_HOME", self.dir.join("remote"))
            .env("HELD_FILE", self.dir.join("held.json"));
        command
    }

    /// No staging directory left on either host.
    fn assert_clean(&self) {
        for host in ["local", "remote"] {
            let leftovers: Vec<_> = fs::read_dir(self.dir.join(host).join("agent"))
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with(".pi-transfuse"))
                .collect();
            assert!(leftovers.is_empty(), "{host} keeps {leftovers:?}");
        }
    }
}

impl Drop for Hosts {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Contents of every file under `root`, by relative path.
fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| (e.path().strip_prefix(root).unwrap().display().to_string(), fs::read(e.path()).unwrap()))
        .collect()
}

fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path).unwrap().lines().map(str::to_string).collect()
}

fn header(path: &Path) -> Value {
    lines(path).into_iter().map(|l| serde_json::from_str::<Value>(&l).unwrap()).find(|v| v["type"] == "session").unwrap()
}

#[test]
fn pull_relocates_and_deletes_the_source() {
    let hosts = Hosts::new();
    let (old, new) = (hosts.work("proj"), hosts.work("new"));
    let source = hosts.session("remote", "s1", &old);
    let before = lines(&source);
    let notes = fs::read(source.with_extension("").join("notes.bin")).unwrap();

    let out = hosts.run(&["migrate", "pull", "box", "s1", "--delete", "--relocate", &new]);
    assert_eq!(out.status.code(), Some(0));

    let landed = hosts.transcript("local", "s1", &new);
    let after = lines(&landed);
    assert_eq!(after.len(), before.len());
    for (i, (a, b)) in before.iter().zip(&after).enumerate() {
        if i != 1 {
            assert_eq!(a, b, "line {i} changed");
        }
    }
    let head = header(&landed);
    assert_eq!(head["cwd"], json!(new));
    assert_eq!(head["additionalDirectories"], json!([format!("{new}/sub"), "/elsewhere"]));
    assert_eq!(header(&landed.with_extension("").join("agents/sub.jsonl"))["cwd"], json!(new));
    assert_eq!(fs::read(landed.with_extension("").join("notes.bin")).unwrap(), notes);
    assert!(landed.with_file_name(format!(".{STAMP}_s1.jsonl.lock.os")).exists());

    assert!(!source.exists() && !source.with_extension("").exists());
    assert!(!hosts.sessions("remote").join(Hosts::bucket(&old)).exists());
    hosts.assert_clean();
}

#[test]
fn push_copies_byte_for_byte_and_refuses_a_second_copy() {
    let hosts = Hosts::new();
    let cwd = hosts.work("proj");
    hosts.session("local", "s2", &cwd);
    let bucket = Hosts::bucket(&cwd);

    let out = hosts.run(&["migrate", "push", "box", "s2"]);
    assert_eq!(out.status.code(), Some(0));
    let local = files(&hosts.sessions("local").join(&bucket));
    assert_eq!(files(&hosts.sessions("remote").join(&bucket)), local);
    assert_eq!(local.len(), 4);

    let again = hosts.run(&["migrate", "push", "box", "s2", "--delete"]);
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stdout).contains("conflict"));
    assert_eq!(files(&hosts.sessions("local").join(&bucket)), local);
    hosts.assert_clean();
}

#[test]
fn a_session_open_at_the_source_stays_put() {
    let hosts = Hosts::new();
    let cwd = hosts.work("proj");
    hosts.session("remote", "s3", &cwd);
    let remote = files(&hosts.sessions("remote"));
    fs::write(hosts.dir.join("held.json"), r#"{"held":["s3"],"unknown":[]}"#).unwrap();

    let out = hosts.run(&["migrate", "pull", "box", "s3", "--delete"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("held"));
    assert_eq!(files(&hosts.sessions("remote")), remote);
    assert!(files(&hosts.sessions("local")).is_empty());
}

/// A session packed in place travels as its archive and lands packed; the
/// project and untitled filters narrow what `--yes` takes.
#[test]
fn packed_sessions_travel_packed_and_filters_narrow_the_choice() {
    let hosts = Hosts::new();
    let (proj, other) = (hosts.work("proj"), hosts.work("new"));
    hosts.session("remote", "p1", &proj);
    hosts.session("remote", "o1", &other);
    let untitled = hosts.session("remote", "u1", &proj);
    let text = fs::read_to_string(&untitled).unwrap().replacen("\"title u1\"", "\"\"", 1);
    fs::write(&untitled, text).unwrap();
    let packed = Command::new(BIN)
        .args(["compact", "pack", "p1", "--older-than", "0d"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", hosts.dir.join("home"))
        .env("OMP_HOME", hosts.dir.join("remote"))
        .output()
        .unwrap();
    assert!(packed.status.success(), "{}", String::from_utf8_lossy(&packed.stderr));
    let bucket = Hosts::bucket(&proj);
    let archive = format!("{STAMP}_p1.tar.gz");
    let bytes = fs::read(hosts.sessions("remote").join(&bucket).join(&archive)).unwrap();

    let out = hosts.run(&["migrate", "pull", "box", "--yes", "--include", "project=proj", "--hide-untitled", "--delete"]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("p1") && !stdout.contains("o1") && !stdout.contains("u1"), "{stdout}");
    assert_eq!(fs::read(hosts.sessions("local").join(&bucket).join(&archive)).unwrap(), bytes);
    assert!(!hosts.transcript("local", "p1", &proj).exists());
    assert!(!hosts.sessions("remote").join(&bucket).join(&archive).exists());
    assert!(hosts.transcript("remote", "u1", &proj).exists() && hosts.transcript("remote", "o1", &other).exists());

    let again = hosts.run(&["migrate", "push", "box", "p1"]);
    assert_eq!(again.status.code(), Some(0));
    assert_eq!(fs::read(hosts.sessions("remote").join(&bucket).join(&archive)).unwrap(), bytes);
    hosts.assert_clean();
}

/// A packed session relocated on the way lands packed in the new bucket, and
/// unpacks there with its cwd rewritten.
#[test]
fn a_relocated_packed_session_is_repacked_into_its_new_bucket() {
    let hosts = Hosts::new();
    let (proj, new) = (hosts.work("proj"), hosts.work("new"));
    hosts.session("remote", "r1", &proj);
    let omp = |home: &str, args: &[&str]| {
        let out = Command::new(BIN)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", hosts.dir.join("home"))
            .env("OMP_HOME", hosts.dir.join(home))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    omp("remote", &["compact", "pack", "r1", "--older-than", "0d"]);

    let out = hosts.run(&["migrate", "pull", "box", "r1", "--relocate", &new]);
    assert_eq!(out.status.code(), Some(0));
    let archive = hosts.sessions("local").join(Hosts::bucket(&new)).join(format!("{STAMP}_r1.tar.gz"));
    assert!(archive.exists());
    omp("local", &["compact", "unpack", "r1"]);
    let landed = hosts.transcript("local", "r1", &new);
    assert_eq!(header(&landed)["cwd"], json!(new));
    assert_eq!(header(&landed.with_extension("").join("agents/sub.jsonl"))["cwd"], json!(new));
    hosts.assert_clean();
}

/// As sshd does: the login shell runs the command with `-c`, in a bare
/// environment, so only rc files a non-interactive shell reads apply.
#[test]
fn the_remote_binary_is_found_through_the_login_shells_rc_files() {
    let hosts = Hosts::new();
    let cwd = hosts.work("proj");
    hosts.session("remote", "s4", &cwd);
    let (rhome, rcbin, mark) = (hosts.dir.join("rhome"), hosts.dir.join("rcbin"), hosts.dir.join("found"));
    fs::create_dir_all(rhome.join(".config/fish")).unwrap();
    fs::create_dir_all(&rcbin).unwrap();
    script(&rcbin.join("pi-transfuse"), &format!("#!/bin/sh\ntouch '{}'\nexec '{BIN}' \"$@\"\n", mark.display()));
    let rc = rcbin.display();
    fs::write(rhome.join(".zshrc"), format!("echo rc noise\npath=({rc} $path)\n")).unwrap();
    fs::write(rhome.join(".bashrc"), format!("echo rc noise\nPATH={rc}:$PATH\n")).unwrap();
    fs::write(rhome.join(".bash_profile"), ". ~/.bashrc\n").unwrap();
    fs::write(rhome.join(".config/fish/config.fish"), format!("set -gx PATH {rc} $PATH\n")).unwrap();
    script(
        &hosts.dir.join("bin/sshd"),
        &format!(
            "#!/bin/sh\nwhile [ \"$1\" != \"--\" ]; do shift; done\nshift 2\n\
             exec env -i HOME='{}' PATH=/usr/bin:/bin SHELL=\"$LOGIN\" OMP_HOME=\"$REMOTE_OMP_HOME\" \
             HELD_FILE=\"$HELD_FILE\" \"$LOGIN\" -c \"$1\"\n",
            rhome.display()
        ),
    );
    let mut tried = 0;
    for shell in ["/bin/zsh", "/bin/bash", "/opt/homebrew/bin/fish", "/usr/bin/fish", "/bin/tcsh"] {
        if !Path::new(shell).exists() {
            continue;
        }
        tried += 1;
        let _ = fs::remove_file(&mark);
        let out = hosts.command(&["migrate", "list", "box", "--json"], "sshd").env("LOGIN", shell).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(0), "{shell}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(stdout.contains("\"s4\""), "{shell}: {stdout}");
        // tcsh cannot be an interactive login shell under -c; the install
        // folders and this program's own path stand in for its rc files.
        if !shell.ends_with("tcsh") {
            assert!(mark.exists(), "{shell} did not run pi-transfuse from the rc PATH");
        }
    }
    assert!(tried >= 2);
}
