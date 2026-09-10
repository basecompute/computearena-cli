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
    pub(crate) scroll: Option<usize>,
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
            receiver,
        }
    }

    pub(crate) fn finished(&self) -> bool {
        self.outcome.is_some()
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
                    if sender.send(JobEvent::Line(line)).is_err() {
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
