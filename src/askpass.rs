//! Brings ssh's questions — passwords, key passphrases, host-key checks —
//! into the browser. ssh runs this same binary as its `SSH_ASKPASS` program;
//! that process hands the question to the browser over a private Unix socket
//! and prints the answer the user typed there.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::{fs, thread};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Names the browser's socket for the askpass process ssh starts.
pub const SOCKET_ENV: &str = "PI_TRANSFUSE_ASKPASS";

#[derive(Serialize, Deserialize)]
struct Ask {
    prompt: String,
    /// `SSH_ASKPASS_PROMPT`: `confirm` for yes/no, `none` for a notice.
    kind: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Answer {
    answer: Option<String>,
}

/// One question ssh asks, waiting for the user.
pub struct Question {
    pub prompt: String,
    /// Typed text stays hidden: a password or passphrase.
    pub secret: bool,
    /// Only shown; ssh carries on without an answer, as for a key-touch notice.
    pub notice: bool,
    reply: Sender<Option<String>>,
}

impl Question {
    /// Answers ssh; `None` declines, which ssh treats as a failed attempt.
    pub fn answer(self, text: Option<String>) {
        let _ = self.reply.send(text);
    }
}

/// The browser's end: a socket in a private directory, answered for the life
/// of the process.
pub struct Server {
    dir: PathBuf,
    pub socket: PathBuf,
}

impl Server {
    /// Listens for askpass processes and passes each question to `questions`.
    pub fn start(questions: Sender<Question>) -> Result<Server> {
        let dir = std::env::temp_dir().join(format!("pi-transfuse-askpass-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let socket = dir.join("sock");
        let listener = UnixListener::bind(&socket).with_context(|| format!("listen on {}", socket.display()))?;
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let questions = questions.clone();
                thread::spawn(move || {
                    let _ = serve(stream, &questions);
                });
            }
        });
        Ok(Server { dir, socket })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn serve(stream: UnixStream, questions: &Sender<Question>) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let ask: Ask = serde_json::from_str(&line)?;
    let (reply, answer) = mpsc::channel();
    let notice = ask.kind.as_deref() == Some("none");
    let secret = !notice && ask.kind.as_deref() != Some("confirm") && !asks_yes_no(&ask.prompt);
    questions.send(Question { prompt: ask.prompt, secret, notice, reply })?;
    // The browser going away declines.
    let answer = Answer { answer: answer.recv().unwrap_or(None) };
    let mut stream = stream;
    serde_json::to_writer(&mut stream, &answer)?;
    stream.write_all(b"\n")?;
    Ok(())
}

/// Host-key checks ask for a typed `yes`, which may show as typed.
fn asks_yes_no(prompt: &str) -> bool {
    prompt.contains("yes/no")
}

/// Askpass mode: asks the browser behind `socket` and prints its answer for
/// ssh. Exit status 1 declines.
pub fn client(socket: &Path) -> i32 {
    let prompt = std::env::args().nth(1).unwrap_or_default();
    let kind = std::env::var("SSH_ASKPASS_PROMPT").ok();
    match ask(socket, Ask { prompt, kind }) {
        Ok(Some(text)) => {
            println!("{text}");
            0
        }
        Ok(None) => 1,
        Err(err) => {
            eprintln!("pi-transfuse askpass: {err:#}");
            1
        }
    }
}

fn ask(socket: &Path, ask: Ask) -> Result<Option<String>> {
    let mut stream = UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    serde_json::to_writer(&mut stream, &ask)?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str::<Answer>(&line)?.answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_reaches_the_browser_and_its_answer_comes_back() {
        let (tx, rx) = mpsc::channel();
        let server = Server::start(tx).unwrap();
        let socket = server.socket.clone();
        let asked = thread::spawn(move || ask(&socket, Ask { prompt: "me@box's password: ".into(), kind: None }));
        let question = rx.recv().unwrap();
        assert!(question.secret && !question.notice);
        assert_eq!(question.prompt, "me@box's password: ");
        question.answer(Some("hunter2".into()));
        assert_eq!(asked.join().unwrap().unwrap().as_deref(), Some("hunter2"));
    }

    #[test]
    fn host_key_checks_show_what_is_typed_and_declining_answers_nothing() {
        let (tx, rx) = mpsc::channel();
        let server = Server::start(tx).unwrap();
        let socket = server.socket.clone();
        let prompt = "Are you sure you want to continue connecting (yes/no/[fingerprint])? ";
        let asked = thread::spawn(move || ask(&socket, Ask { prompt: prompt.into(), kind: None }));
        let question = rx.recv().unwrap();
        assert!(!question.secret);
        question.answer(None);
        assert_eq!(asked.join().unwrap().unwrap(), None);
    }
}
