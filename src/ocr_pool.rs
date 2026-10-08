//! Crash-isolated, parallel text extraction.
//!
//! Vision and ImageIO occasionally die with a hard signal (SIGSEGV, SIGBUS, abort) on a
//! malformed attachment. Nothing in-process can catch that, so extraction runs in
//! disposable `ocr-shard` child processes instead. The parent keeps `N` shards busy from
//! a shared queue (map), and merges their answers back into one stream (reduce). A shard
//! that crashes or hangs costs exactly the one file it was holding: the parent records
//! that file as empty, starts a replacement shard, and carries on.
//!
//! Wire protocol, one request at a time per shard: the parent writes a JSON-encoded path
//! on a line to the shard's stdin; the shard answers with a JSON-encoded text on a line.
//! JSON encoding keeps both sides single-line even for paths or text with newlines.

use std::collections::VecDeque;
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::ffi::recognize_text;
use crate::types::*;

/// The answer for one attachment, as merged back by the pool.
pub struct OcrOutcome {
    pub path: String,
    /// Extracted text; empty when the file had none, could not be read, or took its
    /// shard down with it (crash or timeout). Callers cannot tell those apart, and do
    /// not need to: every case is cached as "no text" against the file's mtime.
    pub text: String,
}

/// How many shards to run. `ocr_workers` in the workflow configuration overrides the
/// default, which is the logical core count capped at `MAX_OCR_WORKERS`.
pub fn ocr_worker_count() -> usize {
    if let Some(requested) = env::var("ocr_workers")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
    {
        if requested >= 1 {
            return requested.min(32);
        }
    }

    thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(2)
        .clamp(1, MAX_OCR_WORKERS)
}

/// Entry point of an `ocr-shard` child process. Answers requests until stdin closes,
/// which is how the parent (or its death) ends the shard.
pub fn run_ocr_shard() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };

        // Every request gets exactly one answer, even an unparseable one, so the parent
        // never waits out the timeout on a protocol slip
        let text = match serde_json::from_str::<String>(&line) {
            Ok(path) => recognize_text(&path),
            Err(_) => String::new(),
        };

        let encoded = serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".to_string());
        if writeln!(out, "{}", encoded).and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

/// One live shard process plus the reader thread that turns its stdout into messages,
/// so the parent can wait on an answer with a timeout.
struct Shard {
    child: Child,
    stdin: ChildStdin,
    answers: Receiver<String>,
}

impl Shard {
    fn spawn() -> Option<Shard> {
        let exe = env::current_exe().ok()?;
        let mut child = Command::new(exe)
            .arg("ocr-shard")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;

        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            // The pipe closed: the shard exited or was killed. Dropping `tx` here is
            // what turns a dead shard into a Disconnected error for the waiting parent.
        });

        Some(Shard { child, stdin, answers: rx })
    }

    /// Sends one path and waits for its text. `None` means the shard is no longer
    /// usable: it died, or it has been silent past `timeout` and should be killed.
    fn request(&mut self, path: &str, timeout: Duration) -> Option<String> {
        let encoded = serde_json::to_string(path).ok()?;
        if writeln!(self.stdin, "{}", encoded).and_then(|_| self.stdin.flush()).is_err() {
            return None;
        }

        match self.answers.recv_timeout(timeout) {
            // A garbled answer is treated as "no text" rather than as a dead shard
            Ok(line) => Some(serde_json::from_str::<String>(&line).unwrap_or_default()),
            Err(_) => None,
        }
    }
}

impl Drop for Shard {
    fn drop(&mut self) {
        // Killing an already-exited child fails harmlessly; the wait reaps it either
        // way so no zombie outlives the pool
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// Owns one shard at a time and drains the shared queue through it, replacing the shard
/// whenever a file takes it down.
fn shard_loop(queue: Arc<Mutex<VecDeque<String>>>, results: Sender<OcrOutcome>, timeout: Duration) {
    let mut shard: Option<Shard> = None;
    let mut spawn_failures: u32 = 0;

    loop {
        let path = match queue.lock().ok().and_then(|mut queue| queue.pop_front()) {
            Some(path) => path,
            None => break,
        };

        if shard.is_none() && spawn_failures < MAX_SHARD_SPAWN_FAILURES {
            shard = Shard::spawn();
            if shard.is_none() {
                spawn_failures += 1;
            }
        }

        let response = shard.as_mut().map(|live| live.request(&path, timeout));
        let text = match response {
            Some(Some(text)) => text,
            Some(None) => {
                // Dropping the shard kills and reaps it; the next iteration starts a new one
                shard = None;
                String::new()
            }
            // Shards cannot be started at all on this machine: isolation is lost, but
            // the index still gets built
            None => recognize_text(&path),
        };

        if results.send(OcrOutcome { path, text }).is_err() {
            break;
        }
    }
}

/// Extracts text from every path using up to `workers` shard processes, calling
/// `on_result` on the current thread as each answer arrives. Completion order is
/// arbitrary. Returns once every path has an outcome.
pub fn run_ocr_pool<F>(paths: Vec<String>, workers: usize, mut on_result: F)
where
    F: FnMut(OcrOutcome),
{
    if paths.is_empty() {
        return;
    }

    let workers = workers.clamp(1, paths.len());
    let queue = Arc::new(Mutex::new(VecDeque::from(paths)));
    let (tx, rx) = mpsc::channel::<OcrOutcome>();
    let timeout = Duration::from_secs(OCR_FILE_TIMEOUT_SECS);

    thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let tx = tx.clone();
            scope.spawn(move || shard_loop(queue, tx, timeout));
        }
        // The loop below ends when the last clone is dropped by the last finishing thread
        drop(tx);

        for outcome in rx {
            on_result(outcome);
        }
    });
}

/// Extracts the text of a single attachment in an isolated shard, for callers that only
/// have one file to read (the inline reconcile path in `main`). A crash yields "".
pub fn recognize_text_isolated(path: &str) -> String {
    let mut text = String::new();
    run_ocr_pool(vec![path.to_string()], 1, |outcome| text = outcome.text);
    text
}
