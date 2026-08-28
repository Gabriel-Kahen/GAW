use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use gaw_core::{Beats, Event, EventData, NoteEvent};

const BASIC_PITCH_EXECUTABLE_ENV: &str = "GAW_BASIC_PITCH";
const BASIC_PITCH_TIMEOUT: Duration = Duration::from_mins(30);

#[derive(Clone, Debug)]
pub(crate) struct TranscriptionJob {
    pub asset_id: gaw_core::AssetId,
    pub content_hash: Option<String>,
    pub source_path: PathBuf,
    pub source_name: String,
    pub bpm: f64,
}

#[derive(Debug)]
pub(crate) struct TranscriptionResult {
    pub job: TranscriptionJob,
    pub event_data: Result<EventData, String>,
}

pub(crate) fn transcribe(
    job: &TranscriptionJob,
    cancelled: &AtomicBool,
) -> Result<EventData, String> {
    let executable = std::env::var_os(BASIC_PITCH_EXECUTABLE_ENV)
        .map_or_else(|| PathBuf::from("basic-pitch"), PathBuf::from);
    transcribe_with_executable(job, cancelled, &executable)
}

fn transcribe_with_executable(
    job: &TranscriptionJob,
    cancelled: &AtomicBool,
    executable: &Path,
) -> Result<EventData, String> {
    if cancelled.load(Ordering::Acquire) {
        return Err("Basic Pitch conversion was cancelled".into());
    }
    let output = tempfile::tempdir().map_err(|error| error.to_string())?;
    let mut child = Command::new(executable)
        .arg(output.path())
        .arg(&job.source_path)
        .arg("--save-note-events")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "Basic Pitch is not installed. Run `uv tool install --python 3.11 --with 'setuptools<81' basic-pitch==0.4.0`, or set {BASIC_PITCH_EXECUTABLE_ENV} to its executable path."
                )
            } else {
                format!("could not start Basic Pitch: {error}")
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
            return Err("Basic Pitch conversion was cancelled".into());
        }
        if started.elapsed() >= BASIC_PITCH_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err("Basic Pitch conversion timed out after 30 minutes".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(format!("could not wait for Basic Pitch: {error}"));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        let detail = last_output_line(&stderr)
            .or_else(|| last_output_line(&stdout))
            .unwrap_or("Basic Pitch exited without an error message");
        return Err(format!("Basic Pitch failed: {detail}"));
    }
    let csv_path = find_note_events(output.path()).map_err(|error| {
        last_output_line(&stdout)
            .or_else(|| last_output_line(&stderr))
            .map_or(error.clone(), |detail| format!("{error}: {detail}"))
    })?;
    parse_note_events(&csv_path, &midi_asset_name(&job.source_name), job.bpm)
}

fn read_bounded_output(mut input: impl std::io::Read) -> Vec<u8> {
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
        .find_map(|line| {
            let line = line.trim();
            (!line.is_empty()).then_some(line)
        })
}

fn find_note_events(directory: &Path) -> Result<PathBuf, String> {
    let mut matches = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "csv"))
        .collect::<Vec<_>>();
    matches.sort();
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err("Basic Pitch did not produce note-event output".into()),
        _ => Err("Basic Pitch produced more than one note-event output".into()),
    }
}

fn parse_note_events(path: &Path, name: &str, bpm: f64) -> Result<EventData, String> {
    if !bpm.is_finite() || bpm <= 0.0 {
        return Err("the transcription tempo must be positive and finite".into());
    }
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut data = EventData::new(name);
    for (index, line) in contents.lines().enumerate().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let columns = line.split(',').collect::<Vec<_>>();
        if columns.len() < 4 {
            return Err(format!(
                "invalid Basic Pitch output on CSV line {}",
                index + 1
            ));
        }
        let start = parse_number(columns[0], "start", index)?;
        let end = parse_number(columns[1], "end", index)?;
        let pitch = columns[2]
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|value| *value <= 127)
            .ok_or_else(|| format!("invalid pitch on CSV line {}", index + 1))?;
        let velocity = columns[3]
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|value| *value <= 127)
            .ok_or_else(|| format!("invalid velocity on CSV line {}", index + 1))?
            .max(1);
        if start < 0.0 || end <= start {
            return Err(format!("invalid note timing on CSV line {}", index + 1));
        }
        let beats_per_second = bpm / 60.0;
        let note = NoteEvent::new(
            Beats::new(start * beats_per_second).map_err(|error| error.to_string())?,
            Beats::new((end - start) * beats_per_second).map_err(|error| error.to_string())?,
            pitch,
            velocity,
        )
        .map_err(|error| error.to_string())?;
        data.events.push(Event::Note(note));
    }
    if data.events.is_empty() {
        return Err("Basic Pitch did not detect any notes".into());
    }
    data.sort();
    Ok(data)
}

fn parse_number(value: &str, label: &str, index: usize) -> Result<f64, String> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("invalid {label} time on CSV line {}", index + 1))
}

pub(crate) fn midi_asset_name(source_name: &str) -> String {
    let source = Path::new(source_name);
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Untitled");
    format!("{stem} (MIDI)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_events_keep_seconds_while_mapping_to_project_beats() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notes.csv");
        fs::write(
            &path,
            "start_time_s,end_time_s,pitch_midi,velocity,pitch_bend\n0.5,1.25,64,96,\n0.0,0.25,60,0,\n",
        )
        .unwrap();
        let data = parse_note_events(&path, "Stem (MIDI)", 120.0).unwrap();
        assert_eq!(data.name, "Stem (MIDI)");
        assert_eq!(data.events.len(), 2);
        let Event::Note(first) = &data.events[0] else {
            panic!("expected a note");
        };
        assert!(first.start.value().abs() < f64::EPSILON);
        assert!((first.duration.value() - 0.5).abs() < f64::EPSILON);
        assert_eq!(first.velocity.value(), 1);
        let Event::Note(second) = &data.events[1] else {
            panic!("expected a note");
        };
        assert!((second.start.value() - 1.0).abs() < f64::EPSILON);
        assert!((second.duration.value() - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn output_name_uses_the_source_stem() {
        assert_eq!(midi_asset_name("guitar.wav"), "guitar (MIDI)");
        assert_eq!(midi_asset_name("bass"), "bass (MIDI)");
    }

    #[test]
    fn empty_note_output_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notes.csv");
        fs::write(
            &path,
            "start_time_s,end_time_s,pitch_midi,velocity,pitch_bend\n",
        )
        .unwrap();
        assert!(
            parse_note_events(&path, "Silence (MIDI)", 120.0)
                .unwrap_err()
                .contains("did not detect any notes")
        );
    }

    #[cfg(unix)]
    #[test]
    fn cli_contract_produces_canonical_note_data() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-basic-pitch");
        fs::write(
            &executable,
            "#!/bin/sh\n[ -f \"$2\" ] || exit 2\n[ \"$3\" = \"--save-note-events\" ] || exit 3\nprintf 'start_time_s,end_time_s,pitch_midi,velocity,pitch_bend\\n0.25,0.75,67,90,\\n' > \"$1/notes.csv\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let source = directory.path().join("source.wav");
        fs::write(&source, []).unwrap();
        let job = TranscriptionJob {
            asset_id: gaw_core::AssetId::new(),
            content_hash: None,
            source_path: source,
            source_name: "Lead.wav".into(),
            bpm: 120.0,
        };

        let data = transcribe_with_executable(&job, &AtomicBool::new(false), &executable).unwrap();
        assert_eq!(data.name, "Lead (MIDI)");
        let Event::Note(note) = &data.events[0] else {
            panic!("expected a note");
        };
        assert!((note.start.value() - 0.5).abs() < f64::EPSILON);
        assert!((note.duration.value() - 1.0).abs() < f64::EPSILON);
    }

    #[cfg(unix)]
    #[test]
    fn zero_exit_without_csv_preserves_basic_pitch_diagnostic() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-basic-pitch");
        fs::write(
            &executable,
            "#!/bin/sh\necho 'NoBackendError: unsupported audio'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let source = directory.path().join("source.wav");
        fs::write(&source, []).unwrap();
        let job = TranscriptionJob {
            asset_id: gaw_core::AssetId::new(),
            content_hash: None,
            source_path: source,
            source_name: "Lead.wav".into(),
            bpm: 120.0,
        };

        let error =
            transcribe_with_executable(&job, &AtomicBool::new(false), &executable).unwrap_err();
        assert!(error.contains("NoBackendError: unsupported audio"));
    }

    #[cfg(unix)]
    #[test]
    fn active_cli_process_is_cancelled_and_reaped() {
        use std::{os::unix::fs::PermissionsExt as _, sync::Arc};

        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-basic-pitch");
        fs::write(&executable, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        let source = directory.path().join("source.wav");
        fs::write(&source, []).unwrap();
        let job = TranscriptionJob {
            asset_id: gaw_core::AssetId::new(),
            content_hash: None,
            source_path: source,
            source_name: "Lead.wav".into(),
            bpm: 120.0,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let started = Instant::now();
        let worker =
            thread::spawn(move || transcribe_with_executable(&job, &worker_cancelled, &executable));

        thread::sleep(Duration::from_millis(100));
        cancelled.store(true, Ordering::Release);
        let error = worker.join().unwrap().unwrap_err();

        assert!(error.contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
