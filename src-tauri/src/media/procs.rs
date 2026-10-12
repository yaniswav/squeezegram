//! Child processes (ffmpeg, ffprobe): no console window, tied to a Job Object so they die with
//! the app, killed on cancellation or timeout, output read without ever blocking the child.

use std::ffi::OsString;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

/// Upper bound of collected stdout (ffprobe JSON is a few KB).
const MAX_COLLECTED: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The process ended by itself.
    Exited,
    TimedOut,
    Cancelled,
}

#[derive(Debug)]
pub struct Output {
    pub ending: Ending,
    /// Exit code as `i32` (negative values are `AVERROR`s); `None` if killed without a code.
    pub code: Option<i32>,
    /// Whole stdout if `Spec::collect_stdout`, else empty.
    pub stdout: String,
    /// Last lines of stderr.
    pub stderr_tail: Vec<String>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.ending == Ending::Exited && self.code == Some(0)
    }
}

pub struct Spec<'a> {
    pub exe: &'a Path,
    pub args: Vec<OsString>,
    pub timeout: Option<Duration>,
    pub cancel: Option<&'a CancellationToken>,
    pub collect_stdout: bool,
    pub stderr_lines: usize,
    /// Runs below normal priority, for work nobody is waiting for.
    pub low_priority: bool,
}

impl<'a> Spec<'a> {
    pub fn new(exe: &'a Path, args: Args) -> Self {
        Self {
            exe,
            args: args.into_vec(),
            timeout: None,
            cancel: None,
            collect_stdout: false,
            stderr_lines: 20,
            low_priority: false,
        }
    }

    /// Below-normal priority class (Windows): the process yields the CPU to the app, to a
    /// running encode and to whatever else the user is doing.
    pub fn low_priority(mut self) -> Self {
        self.low_priority = true;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn cancel(mut self, cancel: &'a CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub fn collect_stdout(mut self) -> Self {
        self.collect_stdout = true;
        self
    }

    /// Lines of stderr kept (`Output::stderr_tail`), 20 by default.
    pub fn stderr_lines(mut self, lines: usize) -> Self {
        self.stderr_lines = lines;
        self
    }

    /// The command line for the journal: executable and arguments (paths included).
    pub fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "exe": self.exe.to_string_lossy(),
            "args": self.args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>(),
        })
    }
}

/// Argument list builder: one element per argument, never a shell string.
#[derive(Debug, Clone, Default)]
pub struct Args(Vec<OsString>);

impl Args {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn arg(mut self, value: impl AsRef<std::ffi::OsStr>) -> Self {
        self.0.push(value.as_ref().to_owned());
        self
    }

    pub fn all(mut self, values: &[&str]) -> Self {
        self.0.extend(values.iter().map(OsString::from));
        self
    }

    pub fn into_vec(self) -> Vec<OsString> {
        self.0
    }
}

/// Runs a process to its end; every stdout line goes to `on_line`.
pub async fn run<F: FnMut(&str)>(spec: Spec<'_>, mut on_line: F) -> std::io::Result<Output> {
    let mut command = Command::new(spec.exe);
    command
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(if spec.low_priority {
        CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS
    } else {
        CREATE_NO_WINDOW
    });

    let mut child = command.spawn()?;
    job::attach(&child);
    let stderr = child.stderr.take();
    let stdout = child.stdout.take();
    let keep = spec.stderr_lines;
    let stderr_task = tokio::spawn(async move {
        match stderr {
            Some(stderr) => tail_lines(stderr, keep).await,
            None => Vec::new(),
        }
    });

    let mut collected = String::new();
    let collect = spec.collect_stdout;
    let work = async {
        if let Some(stdout) = stdout {
            let mut reader = BufReader::new(stdout);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                if reader.read_until(b'\n', &mut buffer).await? == 0 {
                    break;
                }
                let line = String::from_utf8_lossy(&buffer);
                let line = line.trim_end_matches(['\r', '\n']);
                on_line(line);
                if collect && collected.len() < MAX_COLLECTED {
                    collected.push_str(line);
                    collected.push('\n');
                }
            }
        }
        child.wait().await
    };
    let timeout = async {
        match spec.timeout {
            Some(limit) => tokio::time::sleep(limit).await,
            None => std::future::pending().await,
        }
    };
    let cancelled = async {
        match spec.cancel {
            Some(token) => token.cancelled().await,
            None => std::future::pending().await,
        }
    };
    let (ending, status) = tokio::select! {
        status = work => (Ending::Exited, Some(status?)),
        _ = timeout => (Ending::TimedOut, None),
        _ = cancelled => (Ending::Cancelled, None),
    };
    let code = match status {
        Some(status) => status.code(),
        None => {
            kill(&mut child).await;
            None
        }
    };
    // A grandchild holding the pipe open must not hold us: the tail is best effort after a kill.
    let stderr_tail = match tokio::time::timeout(Duration::from_secs(2), stderr_task).await {
        Ok(joined) => joined.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    Ok(Output {
        ending,
        code,
        stdout: collected,
        stderr_tail,
    })
}

async fn kill(child: &mut Child) {
    if let Err(error) = child.kill().await {
        tracing::warn!(event = "process_kill_failed", error = %error, "could not kill a child process");
    }
}

async fn tail_lines(stream: impl AsyncRead + Unpin, keep: usize) -> Vec<String> {
    let mut reader = BufReader::new(stream);
    let mut tail = std::collections::VecDeque::new();
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buffer);
                let line = line.trim_end();
                if line.is_empty() {
                    continue;
                }
                if tail.len() == keep {
                    tail.pop_front();
                }
                tail.push_back(line.to_owned());
            }
        }
    }
    tail.into()
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;

#[cfg(windows)]
mod job {
    //! One Job Object for the whole app, `KILL_ON_JOB_CLOSE`: when the app dies, whatever way,
    //! Windows kills every ffmpeg still running.

    use std::ffi::c_void;
    use std::sync::OnceLock;

    use tokio::process::Child;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows::core::PCWSTR;

    struct Job(HANDLE);

    // SAFETY: a job handle is just a kernel object reference, usable from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    fn create() -> Option<Job> {
        // SAFETY: plain Win32 calls with valid, correctly sized arguments.
        unsafe {
            let handle = CreateJobObjectW(None, PCWSTR::null()).ok()?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast::<c_void>(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .ok()?;
            Some(Job(handle))
        }
    }

    pub fn attach(child: &Child) {
        static JOB: OnceLock<Option<Job>> = OnceLock::new();
        let Some(job) = JOB.get_or_init(create) else {
            tracing::warn!(
                event = "job_object_failed",
                "could not create the Job Object"
            );
            return;
        };
        let Some(process) = child.raw_handle() else {
            return;
        };
        // SAFETY: both handles are valid; the child handle stays open while `child` lives.
        if let Err(error) = unsafe { AssignProcessToJobObject(job.0, HANDLE(process)) } {
            tracing::warn!(event = "job_object_failed", error = %error, "could not attach a child to the Job Object");
        }
    }
}

#[cfg(not(windows))]
mod job {
    pub fn attach(_: &tokio::process::Child) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    // Windows only: `cmd` is always there, and these tests never touch the PATH for ffmpeg.
    fn cmd() -> std::path::PathBuf {
        std::env::var_os("ComSpec")
            .map(Into::into)
            .unwrap_or_else(|| "cmd.exe".into())
    }

    #[tokio::test]
    async fn reads_stdout_lines_stderr_tail_and_exit_code() {
        let exe = cmd();
        let spec = Spec::new(
            &exe,
            Args::new().all(&["/C", "echo one&& echo two&& echo oops 1>&2&& exit 3"]),
        )
        .collect_stdout();
        let mut lines = Vec::new();
        let output = run(spec, |line| lines.push(line.to_owned())).await.unwrap();
        assert_eq!(output.ending, Ending::Exited);
        assert_eq!(output.code, Some(3));
        assert_eq!(lines, ["one", "two"]);
        assert_eq!(output.stdout, "one\ntwo\n");
        assert_eq!(output.stderr_tail, ["oops"]);
        assert!(!output.success());
    }

    /// A long-running process without children (a killed `cmd` would leave its `ping` holding
    /// the pipes).
    fn ping() -> std::path::PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        std::path::PathBuf::from(root)
            .join("System32")
            .join("PING.EXE")
    }

    #[tokio::test]
    async fn timeout_kills_the_process() {
        let exe = ping();
        let spec = Spec::new(&exe, Args::new().all(&["-n", "30", "127.0.0.1"]))
            .timeout(Duration::from_millis(300));
        let started = std::time::Instant::now();
        let output = run(spec, |_| {}).await.unwrap();
        assert_eq!(output.ending, Ending::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn cancellation_kills_the_process() {
        let exe = ping();
        let token = CancellationToken::new();
        let spec = Spec::new(&exe, Args::new().all(&["-n", "30", "127.0.0.1"])).cancel(&token);
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            canceller.cancel();
        });
        let started = std::time::Instant::now();
        let output = run(spec, |_| {}).await.unwrap();
        assert_eq!(output.ending, Ending::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_missing_executable_is_an_io_error() {
        let exe = std::path::PathBuf::from("Z:\\definitely\\not\\here.exe");
        assert!(run(Spec::new(&exe, Args::new()), |_| {}).await.is_err());
    }
}
