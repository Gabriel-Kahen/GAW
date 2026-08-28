use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

const XLANCE_EXECUTABLE_ENV: &str = "GAW_XLANCE";
const XLANCE_PYTHON_ENV: &str = "GAW_XLANCE_PYTHON";
const XLANCE_TIMEOUT: Duration = Duration::from_hours(6);
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
    pub source_name: String,
    pub options: StemSplitOptions,
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
    if cancelled.load(Ordering::Acquire) {
        return Err("X-LANCE stem split was cancelled".into());
    }
    let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if job.options.denoise {
        command.arg("--denoise");
    }
    if job.options.dereverb_vocals && job.options.stems.contains(&Stem::Vocals) {
        command.arg("--dereverb-vocals");
    }
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
    let stdout_reader = thread::spawn(move || stdout.map_or_else(Vec::new, read_bounded_output));
    let stderr = child.stderr.take();
    let stderr_reader = thread::spawn(move || stderr.map_or_else(Vec::new, read_bounded_output));
    let started = Instant::now();
    let status = loop {
        if cancelled.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("X-LANCE stem split was cancelled".into());
        }
        if started.elapsed() >= XLANCE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("X-LANCE stem split timed out after 6 hours".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
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

    #[test]
    fn exposes_the_eight_xlance_targets_in_display_order() {
        assert_eq!(Stem::ALL.len(), 8);
        assert_eq!(Stem::ALL[0].label(), "VOCALS");
        assert_eq!(Stem::ALL[7].label(), "ORCHESTRAL ELEMENTS");
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
            source_name: "song.wav".into(),
            options: StemSplitOptions {
                stems: vec![Stem::Vocals, Stem::Drums],
                denoise: true,
                dereverb_vocals: true,
            },
        };
        let output = split_with_executable(&job, &AtomicBool::new(false), &executable).unwrap();
        assert_eq!(output.files.len(), 2);
        assert_eq!(output.files[0].stem, Stem::Vocals);
        assert_eq!(output.files[1].stem, Stem::Drums);
    }
}
