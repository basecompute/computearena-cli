//! Long operations run on a worker thread while the interface keeps drawing.
//!
//! The command-line flows already print exactly the right progress lines, so
//! rather than duplicating them the worker redirects the process's stdout and
//! stderr into a pipe for the duration of the job and streams what arrives into
//! the log pane. Drawing is unaffected: the terminal writes through a duplicate
//! of the original descriptor taken before any redirection (see `tty`).
use anyhow::Result;
use std::io::{BufRead, BufReader};
use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Instant;

pub(crate) enum JobEvent {
    Line(String),
    Finished(Result<String, String>),
}

/// What a finished job leaves behind for the screen that started it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobKind {
    Benchmark,
    Download,
    Install,
    Login,
    Logout,
    Submit,
    Verify,
    List,
}

pub(crate) struct Job {
    pub(crate) kind: JobKind,
    pub(crate) title: String,
    pub(crate) log: Vec<String>,
    pub(crate) started: Instant,
    pub(crate) outcome: Option<Result<String, String>>,
    /// Index of the top visible log line; `None` follows the newest output.
    pub(crate) scroll: Option<usize>,
    /// How many log lines the pane last had room for, recorded while drawing
    /// so scrolling can stop at the top instead of emptying the pane.
    pub(crate) visible: usize,
    receiver: Receiver<JobEvent>,
}

impl Job {
    pub(crate) fn spawn<F>(kind: JobKind, title: impl Into<String>, work: F) -> Self
    where
        F: FnOnce() -> Result<String> + Send + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        let lines = sender.clone();
        thread::spawn(move || {
            let capture = Capture::start(lines);
            let outcome = work().map_err(|error| format!("{error:#}"));
            drop(capture);
            let _ = sender.send(JobEvent::Finished(outcome));
        });
        Self {
            kind,
            title: title.into(),
            log: Vec::new(),
            started: Instant::now(),
            outcome: None,
            scroll: None,
            visible: 1,
            receiver,
        }
    }

    pub(crate) fn finished(&self) -> bool {
        self.outcome.is_some()
    }

    /// A job with no worker behind it, for tests about presentation only:
    /// spawning one redirects this process's output, which would take the
    /// output of any test running beside it.
    #[cfg(test)]
    fn detached(log: Vec<String>, visible: usize) -> Self {
        let (_, receiver) = mpsc::channel();
        Self {
            kind: JobKind::List,
            title: String::new(),
            log,
            started: Instant::now(),
            outcome: None,
            scroll: None,
            visible,
            receiver,
        }
    }

    /// The top line when following the newest output.
    pub(crate) fn tail_top(&self) -> usize {
        self.log.len().saturating_sub(self.visible.max(1))
    }

    /// Move the window by `delta` lines, stopping at the first line and
    /// resuming following once it reaches the newest.
    pub(crate) fn scroll_by(&mut self, delta: isize) {
        let tail_top = self.tail_top();
        let top = self.scroll.unwrap_or(tail_top).saturating_add_signed(delta);
        self.scroll = if top >= tail_top { None } else { Some(top) };
    }

    /// Drain whatever the worker produced since the last frame. Returns true
    /// when anything changed, so the caller only redraws when it must.
    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.receiver.try_recv() {
                Ok(JobEvent::Line(line)) => {
                    self.log.push(line);
                    changed = true;
                }
                Ok(JobEvent::Finished(outcome)) => {
                    self.outcome = Some(outcome);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.outcome.is_none() {
                        self.outcome = Some(Err("the worker stopped unexpectedly".to_string()));
                        changed = true;
                    }
                    break;
                }
            }
        }
        changed
    }
}

/// Redirects file descriptors 1 and 2 into a pipe, restoring them on drop.
/// Child processes inherit the redirection, so a runtime's own output lands in
/// the same log as ComputeArena's.
struct Capture {
    stdout: RawFd,
    stderr: RawFd,
    reader: Option<thread::JoinHandle<()>>,
}

impl Capture {
    fn start(sender: Sender<JobEvent>) -> Option<Self> {
        // SAFETY: every call takes descriptors this function owns or created,
        // and each raw descriptor is closed exactly once (here or on drop).
        unsafe {
            let mut pipe = [0 as libc::c_int; 2];
            if libc::pipe(pipe.as_mut_ptr()) != 0 {
                return None;
            }
            let (read, write) = (pipe[0], pipe[1]);
            let stdout = libc::dup(libc::STDOUT_FILENO);
            let stderr = libc::dup(libc::STDERR_FILENO);
            if stdout < 0 || stderr < 0 {
                libc::close(read);
                libc::close(write);
                return None;
            }
            libc::dup2(write, libc::STDOUT_FILENO);
            libc::dup2(write, libc::STDERR_FILENO);
            libc::close(write);
            let source = std::fs::File::from_raw_fd(read);
            let reader = thread::spawn(move || {
                for line in BufReader::new(source).lines() {
                    let Ok(line) = line else { break };
                    if sender.send(JobEvent::Line(plain_text(&line))).is_err() {
                        break;
                    }
                }
            });
            Some(Self {
                stdout,
                stderr,
                reader: Some(reader),
            })
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: the saved descriptors were created by `start` and are closed
        // once, after the originals have been put back.
        unsafe {
            libc::dup2(self.stdout, libc::STDOUT_FILENO);
            libc::dup2(self.stderr, libc::STDERR_FILENO);
            libc::close(self.stdout);
            libc::close(self.stderr);
        }
        // Closing the write ends ends the reader's loop; wait so no line is
        // lost between the job finishing and the log being shown as complete.
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// What a captured line looks like once terminal control is taken out of it.
/// A runtime that believes it is talking to a terminal colours its output and
/// rewrites progress in place with carriage returns; the drawing layer drops
/// the escape byte and prints the rest as text, so `[2K` and `[1;38;2;…m`
/// would litter the log pane. Escape sequences go, a carriage return keeps
/// only what was written after it (the state the terminal would have shown),
/// tabs become spaces, and other control characters are dropped.
pub(crate) fn plain_text(line: &str) -> String {
    let mut text = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\x1b' => match chars.next() {
                // CSI: parameters and intermediates, then one final byte.
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ESC \ (the ESC is consumed here, the
                // backslash by the next iteration as an ordinary character
                // is avoided by checking for it).
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // Two-byte escapes (charset selection, keypad modes, ...).
                Some(_) | None => {}
            },
            '\r' => text.clear(),
            '\t' => text.push_str("    "),
            character if character.is_control() => {}
            character => text.push(character),
        }
    }
    text
}

/// A writable handle to the controlling terminal that survives the redirection
/// above, used as the drawing target for the whole session.
pub(crate) fn tty() -> std::io::Result<std::fs::File> {
    let stdout = std::io::stdout();
    // SAFETY: `dup` returns a fresh descriptor owned by the returned File.
    let duplicate = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(&stdout)) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `duplicate` is a valid descriptor this function just created.
    Ok(unsafe { std::fs::File::from_raw_fd(duplicate) })
}

/// Keeps `IntoRawFd` referenced for platforms where the helper is unused.
#[allow(dead_code)]
fn _assert_traits(file: std::fs::File) -> RawFd {
    file.into_raw_fd()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn terminal_control_is_taken_out_of_captured_lines() {
        assert_eq!(
            plain_text("\x1b[1;38;2;195;255;77m→ pp128 · warmup\x1b[0m"),
            "→ pp128 · warmup"
        );
        // In-place progress: only the final state survives a carriage return.
        assert_eq!(
            plain_text("\r\x1b[2K→ pp128 · at least 3s\r\x1b[2K✓ pp128 · 417.70 tok/s"),
            "✓ pp128 · 417.70 tok/s"
        );
        assert_eq!(plain_text("\x1b]0;title\x07after"), "after");
        assert_eq!(plain_text("\x1b]0;title\x1b\\after"), "after");
        assert_eq!(plain_text("a\tb\x07c"), "a    bc");
        assert_eq!(plain_text("plain ✓ text"), "plain ✓ text");
    }

    #[test]
    fn scrolling_stops_at_the_first_line_and_resumes_following_at_the_newest() {
        let mut job = Job::detached((0..10).map(|line| line.to_string()).collect(), 4);
        assert_eq!(job.tail_top(), 6);

        job.scroll_by(-2);
        assert_eq!(job.scroll, Some(4));
        // Far past the top stops at the first line, keeping a full window.
        job.scroll_by(-100);
        assert_eq!(job.scroll, Some(0));
        // Reaching the newest line resumes following, so new output shows.
        job.scroll_by(100);
        assert_eq!(job.scroll, None);

        // A log shorter than the window has nothing to scroll.
        job.log.truncate(2);
        job.scroll_by(-5);
        assert_eq!(job.scroll, None);
    }

    #[test]
    fn a_job_streams_what_the_work_prints_and_then_its_summary() {
        // Written through the descriptor rather than `println!`, which the
        // test harness captures per-thread before it reaches the pipe. In the
        // binary the macros write to the same descriptor this redirects.
        let mut job = Job::spawn(JobKind::List, "printing", || {
            use std::io::Write as _;
            let mut out = std::io::stdout();
            out.write_all(b"first line\nsecond line\n")?;
            out.flush()?;
            Ok("done".to_string())
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !job.finished() && Instant::now() < deadline {
            job.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        job.poll();
        assert_eq!(job.outcome.as_ref().unwrap().as_deref(), Ok("done"));
        assert!(
            job.log.iter().any(|line| line == "first line"),
            "log was {:?}",
            job.log
        );
        assert!(job.log.iter().any(|line| line == "second line"));
    }
}
