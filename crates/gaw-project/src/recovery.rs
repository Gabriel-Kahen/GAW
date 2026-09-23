use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Read, Seek, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use gaw_core::Transaction;

use crate::{Result, SCHEMA_VERSION, error::io};

/// One committed command group awaiting a canonical snapshot.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRecord {
    pub schema_version: u32,
    pub sequence: u64,
    pub committed_at_unix_ms: u64,
    pub before_snapshot_hash: String,
    pub after_snapshot_hash: String,
    pub transaction: Transaction,
}

pub(crate) fn read(path: &Path) -> Result<Vec<RecoveryRecord>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io(path, error)),
    };
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .map_err(|error| io(path, error))?;
    let mut records = Vec::new();
    // Only newline-terminated records are committed; leave a torn final record unread.
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let Some(line) = line.strip_suffix(b"\n") else {
            break;
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: RecoveryRecord =
            serde_json::from_slice(line).map_err(|source| crate::Error::Json {
                path: path.to_owned(),
                source,
            })?;
        crate::format::check_schema(record.schema_version.into())?;
        let expected = records
            .last()
            .map_or(1, |previous: &RecoveryRecord| previous.sequence + 1);
        if record.sequence != expected {
            return Err(crate::Error::InvalidTransaction(format!(
                "recovery sequence {} follows {}, expected {expected}",
                record.sequence,
                expected.saturating_sub(1)
            )));
        }
        for hash in [&record.before_snapshot_hash, &record.after_snapshot_hash] {
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(crate::Error::InvalidTransaction(
                    "recovery snapshot hash must be 64 lowercase hexadecimal characters".into(),
                ));
            }
        }
        if let Some(previous) = records.last()
            && previous.after_snapshot_hash != record.before_snapshot_hash
        {
            return Err(crate::Error::InvalidTransaction(
                "recovery snapshot hash chain is broken".into(),
            ));
        }
        records.push(record);
    }
    Ok(records)
}

pub(crate) fn append(
    path: &Path,
    transaction: &Transaction,
    before_snapshot_hash: String,
    after_snapshot_hash: String,
) -> Result<RecoveryRecord> {
    let records = read(path)?;
    let sequence = records.last().map_or(1, |record| record.sequence + 1);
    let committed_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let record = RecoveryRecord {
        schema_version: SCHEMA_VERSION,
        sequence,
        committed_at_unix_ms,
        before_snapshot_hash,
        after_snapshot_hash,
        transaction: transaction.clone(),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| io(path, error))?;
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .map_err(|error| io(path, error))?;
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        let valid_len = contents
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1);
        file.set_len(u64::try_from(valid_len).unwrap_or(u64::MAX))
            .map_err(|error| io(path, error))?;
    }
    file.seek(std::io::SeekFrom::End(0))
        .map_err(|error| io(path, error))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, &record).map_err(|source| crate::Error::Json {
        path: path.to_owned(),
        source,
    })?;
    // Flush record bytes before its commit newline and the existing durability syncs.
    writer.flush().map_err(|source| crate::Error::Json {
        path: path.to_owned(),
        source: serde_json::Error::io(source),
    })?;
    let file = writer.get_mut();
    file.write_all(b"\n").map_err(|error| io(path, error))?;
    file.sync_data().map_err(|error| io(path, error))?;
    sync_parent(path)?;
    Ok(record)
}

pub(crate) fn clear(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io(path, error)),
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io(parent, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gaw_core::{Command, Event, EventData, NoteEvent};

    #[test]
    fn journal_read_preserves_complete_records_at_every_truncation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        let transaction = Transaction::new([Command::SetProjectName {
            name: "résumé\n🎹".into(),
        }]);
        let first = append(&path, &transaction, "0".repeat(64), "1".repeat(64)).unwrap();
        let first_bytes = fs::read(&path).unwrap();
        let second = append(&path, &transaction, "1".repeat(64), "2".repeat(64)).unwrap();
        let complete = fs::read(&path).unwrap();
        let expected = serde_json::to_value([&first, &second]).unwrap();
        for end in 0..=complete.len() {
            fs::write(&path, &complete[..end]).unwrap();
            let read = read(&path).unwrap();
            let count = usize::from(end >= first_bytes.len()) + usize::from(end == complete.len());
            assert_eq!(
                serde_json::to_value(read)
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .as_slice(),
                &expected.as_array().unwrap()[..count]
            );
        }
        let mut whitespace = b" \t\r\n\n".to_vec();
        whitespace.extend_from_slice(&complete);
        whitespace.extend_from_slice(b"\r\n\t");
        fs::write(&path, whitespace).unwrap();
        assert_eq!(
            serde_json::to_value(read(&path).unwrap()).unwrap(),
            expected
        );
    }

    #[test]
    fn buffered_journal_append_preserves_bytes_and_repairs_a_torn_tail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        let transaction = Transaction::new([Command::SetProjectName {
            name: "x\n🎹".repeat(8_192),
        }]);
        let first = append(&path, &transaction, "0".repeat(64), "1".repeat(64)).unwrap();
        let mut expected = serde_json::to_vec(&first).unwrap();
        expected.push(b'\n');
        assert_eq!(fs::read(&path).unwrap(), expected);
        let mut torn = expected.clone();
        torn.extend_from_slice(b"{\"schema_version\":");
        fs::write(&path, torn).unwrap();
        let second = append(&path, &transaction, "1".repeat(64), "2".repeat(64)).unwrap();
        expected.extend(serde_json::to_vec(&second).unwrap());
        expected.push(b'\n');
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert_eq!(read(&path).unwrap().len(), 2);
    }

    #[test]
    #[ignore = "manual durable journal append performance measurement"]
    fn benchmark_dense_journal_append() {
        use std::{hint::black_box, time::Instant};
        let beats = |value| gaw_core::Beats::new(value).unwrap();
        let mut data = EventData::new("Dense notes");
        data.events = (0..10_000)
            .map(|index| {
                Event::Note(
                    NoteEvent::new(beats(f64::from(index) * 0.25), beats(0.125), 60, 100).unwrap(),
                )
            })
            .collect();
        let transaction = Transaction::new([Command::AddEventData { event_data: data }]);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        let mut times = Vec::new();
        for _ in 0..5 {
            let _ = fs::remove_file(&path);
            let started = Instant::now();
            let record = append(
                &path,
                black_box(&transaction),
                "0".repeat(64),
                "1".repeat(64),
            )
            .unwrap();
            times.push(started.elapsed());
            let mut expected = serde_json::to_vec(&record).unwrap();
            expected.push(b'\n');
            assert_eq!(fs::read(&path).unwrap(), expected);
        }
        times.sort_unstable();
        eprintln!("durable 10,000-note journal append: {:?} median", times[2]);
    }
}
