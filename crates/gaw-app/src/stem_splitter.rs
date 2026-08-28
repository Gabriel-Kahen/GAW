use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

const XLANCE_EXECUTABLE_ENV: &str = "GAW_XLANCE";
const XLANCE_PYTHON_ENV: &str = "GAW_XLANCE_PYTHON";
const DEFAULT_XLANCE_TIMEOUT: Duration = Duration::from_hours(6);
const BUNDLED_ADAPTER: &str = include_str!("../../../scripts/gaw-xlance");

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Stem {
    Vocals,
    Guitars,
    Keyboards,
    Bass,
    Synthesizers,
    Drums,
    Percussions,
    Orchestral,
}

impl Stem {
    pub(crate) const ALL: [Self; 8] = [
        Self::Vocals,
        Self::Guitars,
        Self::Keyboards,
        Self::Bass,
        Self::Synthesizers,
        Self::Drums,
        Self::Percussions,
        Self::Orchestral,
    ];

    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Vocals => "vox",
            Self::Guitars => "gtr",
            Self::Keyboards => "key",
            Self::Bass => "bass",
            Self::Synthesizers => "syn",
            Self::Drums => "drums",
            Self::Percussions => "perc",
            Self::Orchestral => "orch",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Vocals => "VOCALS",
            Self::Guitars => "GUITARS",
            Self::Keyboards => "KEYBOARDS",
            Self::Bass => "BASS",
            Self::Synthesizers => "SYNTHESIZERS",
            Self::Drums => "DRUMS",
            Self::Percussions => "PERCUSSIONS",
            Self::Orchestral => "ORCHESTRAL ELEMENTS",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct StemSplitOptions {
    pub stems: Vec<Stem>,
    pub denoise: bool,
    pub dereverb_vocals: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct StemSplitJob {
    pub asset_id: gaw_core::AssetId,
    pub content_hash: Option<String>,
    pub source_path: PathBuf,
    pub workspace_root: PathBuf,
    pub source_name: String,
    pub options: StemSplitOptions,
    pub cancelled: Arc<AtomicBool>,
    pub completed_stems: Arc<AtomicUsize>,
}

#[derive(Debug)]
pub(crate) struct StemFile {
    pub stem: Stem,
    pub path: PathBuf,
}

#[derive(Debug)]
pub(crate) struct StemSplitOutput {
    _directory: tempfile::TempDir,
    pub files: Vec<StemFile>,
}

#[cfg(test)]
impl StemSplitOutput {
    pub(crate) fn from_test_files(files: &[(Stem, &Path)]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let files = files
            .iter()
            .map(|(stem, source)| {
                let path = directory.path().join(format!("{}.wav", stem.key()));
                fs::copy(source, &path).unwrap();
                StemFile { stem: *stem, path }
            })
            .collect();
        Self {
            _directory: directory,
            files,
        }
    }
}

#[derive(Debug)]
pub(crate) struct StemSplitResult {
    pub job: StemSplitJob,
    pub output: Result<StemSplitOutput, String>,
}

pub(crate) fn split(job: &StemSplitJob, cancelled: &AtomicBool) -> Result<StemSplitOutput, String> {
    #[cfg(not(target_os = "linux"))]
    return Err("the bundled X-LANCE integration currently requires Linux and NVIDIA CUDA".into());

    #[cfg(target_os = "linux")]
    split_with_command(job, cancelled, None)
}

#[cfg(test)]
fn split_with_executable(
    job: &StemSplitJob,
    cancelled: &AtomicBool,
    executable: &Path,
) -> Result<StemSplitOutput, String> {
    split_with_command(job, cancelled, Some(executable))
}

fn split_with_command(
    job: &StemSplitJob,
    cancelled: &AtomicBool,
    executable: Option<&Path>,
) -> Result<StemSplitOutput, String> {
    if job.options.stems.is_empty() {
        return Err("select at least one X-LANCE stem".into());
    }
    if is_cancelled(job, cancelled) {
        return Err("X-LANCE stem split was cancelled".into());
    }
    fs::create_dir_all(&job.workspace_root).map_err(|error| {
        format!(
            "could not create the X-LANCE workspace {}: {error}",
            job.workspace_root.display()
        )
    })?;
    preflight_workspace(job)?;
    let directory = tempfile::Builder::new()
        .prefix("job-")
        .tempdir_in(&job.workspace_root)
        .map_err(|error| error.to_string())?;
    let mut command = adapter_command(directory.path(), executable)?;
    let stems = job
        .options
        .stems
        .iter()
        .map(|stem| stem.key())
        .collect::<Vec<_>>()
        .join(",");
    command
        .arg("split")
        .arg("--input")
        .arg(&job.source_path)
        .arg("--output-dir")
        .arg(directory.path())
        .arg("--stems")
        .arg(stems)
        .env("TMPDIR", directory.path())
        .env("TMP", directory.path())
        .env("TEMP", directory.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if job.options.denoise {
        command.arg("--denoise");
    }
    if job.options.dereverb_vocals && job.options.stems.contains(&Stem::Vocals) {
        command.arg("--dereverb-vocals");
    }
    run_command(&mut command, job, cancelled)?;

    let files = job
        .options
        .stems
        .iter()
        .map(|stem| {
            let path = directory.path().join(format!("{}.wav", stem.key()));
            if !path.is_file() {
                return Err(format!("X-LANCE did not produce the {} stem", stem.label()));
            }
            fs::canonicalize(&path)
                .map(|path| StemFile { stem: *stem, path })
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(StemSplitOutput {
        _directory: directory,
        files,
    })
}

fn run_command(
    command: &mut Command,
    job: &StemSplitJob,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "X-LANCE is not installed. Install its Python dependencies and set {XLANCE_PYTHON_ENV} to that environment's Python, or set {XLANCE_EXECUTABLE_ENV} to a compatible adapter."
            )
        } else {
            format!("could not start X-LANCE: {error}")
        }
    })?;
    let stdout = child.stdout.take();
    let completed_stems = Arc::clone(&job.completed_stems);
    let stdout_reader = thread::spawn(move || {
        stdout.map_or_else(Vec::new, |stdout| {
            read_progress_output(stdout, &completed_stems)
        })
    });
    let stderr = child.stderr.take();
    let stderr_reader = thread::spawn(move || stderr.map_or_else(Vec::new, read_bounded_output));
    let started = Instant::now();
    let timeout = xlance_timeout();
    let status = loop {
        if is_cancelled(job, cancelled) {
            terminate_process_tree(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("X-LANCE stem split was cancelled".into());
        }
        if started.elapsed() >= timeout {
            terminate_process_tree(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!(
                "X-LANCE stem split timed out after {} hours",
                timeout.as_secs() / 3_600
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                terminate_process_tree(&mut child);
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("could not wait for X-LANCE: {error}"));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let detail = last_output_line(&stderr)
            .or_else(|| last_output_line(&stdout))
            .unwrap_or("X-LANCE exited without an error message");
        return Err(format!("X-LANCE failed: {detail}"));
    }
    Ok(())
}

fn preflight_workspace(job: &StemSplitJob) -> Result<(), String> {
    const RESERVE: u64 = 512 * 1024 * 1024;
    let source_bytes = fs::metadata(&job.source_path)
        .map_err(|error| format!("could not inspect X-LANCE input: {error}"))?
        .len();
    let copies = u64::try_from(job.options.stems.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(2)
        .saturating_add(3);
    let required = source_bytes.saturating_mul(copies).saturating_add(RESERVE);
    let available = fs2::available_space(&job.workspace_root)
        .map_err(|error| format!("could not check X-LANCE workspace capacity: {error}"))?;
    if available < required {
        let required_tenths = required.saturating_mul(10) / 1024_u64.pow(3);
        let available_tenths = available.saturating_mul(10) / 1024_u64.pow(3);
        return Err(format!(
            "X-LANCE needs about {}.{} GiB of free project storage for this split, but only {}.{} GiB is available",
            required_tenths / 10,
            required_tenths % 10,
            available_tenths / 10,
            available_tenths % 10,
        ));
    }
    Ok(())
}

fn is_cancelled(job: &StemSplitJob, worker_cancelled: &AtomicBool) -> bool {
    worker_cancelled.load(Ordering::Acquire) || job.cancelled.load(Ordering::Acquire)
}

fn xlance_timeout() -> Duration {
    std::env::var("GAW_XLANCE_TIMEOUT_HOURS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|hours| *hours > 0)
        .and_then(|hours| hours.checked_mul(3_600))
        .map_or(DEFAULT_XLANCE_TIMEOUT, Duration::from_secs)
}

fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    // SAFETY: the child was placed in a new process group whose ID is its PID.
    // Sending SIGKILL to the negative ID targets only that group.
    unsafe {
        let _ = libc::kill(-child.id().cast_signed(), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

fn adapter_command(directory: &Path, executable: Option<&Path>) -> Result<Command, String> {
    if let Some(executable) = executable
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(XLANCE_EXECUTABLE_ENV).map(PathBuf::from))
    {
        Ok(Command::new(executable))
    } else {
        let python = std::env::var_os(XLANCE_PYTHON_ENV)
            .map_or_else(|| PathBuf::from("python3"), PathBuf::from);
        let adapter = directory.join("gaw-xlance");
        fs::write(&adapter, BUNDLED_ADAPTER).map_err(|error| {
            format!("could not materialize the bundled X-LANCE adapter: {error}")
        })?;
        let mut command = Command::new(python);
        command.arg(adapter);
        Ok(command)
    }
}

fn read_bounded_output(mut input: impl Read) -> Vec<u8> {
    const LIMIT: usize = 256 * 1024;
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    while let Ok(read) = input.read(&mut chunk) {
        if read == 0 {
            break;
        }
        output.extend_from_slice(&chunk[..read]);
        if output.len() > LIMIT {
            output.drain(..output.len() - LIMIT);
        }
    }
    output
}

fn read_progress_output(input: impl Read, completed_stems: &AtomicUsize) -> Vec<u8> {
    const LIMIT: usize = 256 * 1024;
    let mut output = Vec::new();
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        if serde_json::from_slice::<serde_json::Value>(&line)
            .ok()
            .and_then(|value| {
                value
                    .get("event")
                    .and_then(|event| event.as_str())
                    .map(str::to_owned)
            })
            .as_deref()
            == Some("stem_complete")
        {
            completed_stems.fetch_add(1, Ordering::Release);
        }
        output.extend_from_slice(&line);
        if output.len() > LIMIT {
            output.drain(..output.len() - LIMIT);
        }
    }
    output
}

fn last_output_line(output: &[u8]) -> Option<&str> {
    std::str::from_utf8(output)
        .ok()?
        .lines()
        .rev()
        .find_map(|line| (!line.trim().is_empty()).then_some(line.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn exposes_the_eight_xlance_targets_in_display_order() {
        assert_eq!(Stem::ALL.len(), 8);
        assert_eq!(Stem::ALL[0].label(), "VOCALS");
        assert_eq!(Stem::ALL[7].label(), "ORCHESTRAL ELEMENTS");
    }

    #[test]
    fn progress_reader_counts_completed_stems() {
        let completed = AtomicUsize::new(0);
        let output = read_progress_output(
            Cursor::new(b"loading\n{\"event\":\"stem_complete\",\"stem\":\"vox\"}\n".as_slice()),
            &completed,
        );
        assert_eq!(completed.load(Ordering::Acquire), 1);
        assert!(String::from_utf8(output).unwrap().contains("stem_complete"));
    }

    #[cfg(unix)]
    #[test]
    fn executable_contract_collects_only_requested_outputs() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-xlance");
        fs::write(
            &executable,
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --output-dir) out=$2; shift 2 ;;\n    --stems) stems=$2; shift 2 ;;\n    *) shift ;;\n  esac\ndone\nmkdir -p \"$out\"\nprintf '%s\\n' \"$stems\" | tr ',' '\\n' | while read stem; do : > \"$out/$stem.wav\"; done\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let source = directory.path().join("song.wav");
        fs::write(&source, []).unwrap();
        let job = StemSplitJob {
            asset_id: gaw_core::AssetId::new(),
            content_hash: None,
            source_path: source,
            workspace_root: directory.path().join("workspace"),
            source_name: "song.wav".into(),
            options: StemSplitOptions {
                stems: vec![Stem::Vocals, Stem::Drums],
                denoise: true,
                dereverb_vocals: true,
            },
            cancelled: Arc::new(AtomicBool::new(false)),
            completed_stems: Arc::new(AtomicUsize::new(0)),
        };
        let output = split_with_executable(&job, &AtomicBool::new(false), &executable).unwrap();
        assert_eq!(output.files.len(), 2);
        assert_eq!(output.files[0].stem, Stem::Vocals);
        assert_eq!(output.files[1].stem, Stem::Drums);
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_the_adapter_process_group() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("slow-xlance");
        fs::write(&executable, "#!/bin/sh\nsleep 30 &\nwait\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let source = directory.path().join("song.wav");
        fs::write(&source, []).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let job = StemSplitJob {
            asset_id: gaw_core::AssetId::new(),
            content_hash: None,
            source_path: source,
            workspace_root: directory.path().join("workspace"),
            source_name: "song.wav".into(),
            options: StemSplitOptions {
                stems: vec![Stem::Vocals],
                denoise: false,
                dereverb_vocals: false,
            },
            cancelled: Arc::clone(&cancelled),
            completed_stems: Arc::new(AtomicUsize::new(0)),
        };
        let started = Instant::now();
        let worker_cancelled = Arc::new(AtomicBool::new(false));
        let handle =
            thread::spawn(move || split_with_executable(&job, &worker_cancelled, &executable));
        thread::sleep(Duration::from_millis(200));
        cancelled.store(true, Ordering::Release);

        assert!(handle.join().unwrap().unwrap_err().contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
