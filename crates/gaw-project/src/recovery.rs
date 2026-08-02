use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{JsonTransaction, Result, SCHEMA_VERSION, error::io};

/// One committed command group awaiting a canonical snapshot.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecoveryRecord {
    pub schema_version: u32,
    pub sequence: u64,
    pub committed_at_unix_ms: u64,
    pub before_snapshot_hash: String,
    pub after_snapshot_hash: String,
    pub transaction: JsonTransaction,
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
    let terminated = contents.ends_with(b"\n");
    let lines = contents.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let complete_lines = if terminated {
        lines.len()
    } else {
        lines.len().saturating_sub(1)
    };
    for line in lines.into_iter().take(complete_lines) {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let record: RecoveryRecord =
            serde_json::from_slice(line).map_err(|source| crate::Error::Json {
                path: path.to_owned(),
                source,
            })?;
        crate::store::check_schema(record.schema_version.into())?;
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
    transaction: &JsonTransaction,
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
        .append(true)
        .open(path)
        .map_err(|error| io(path, error))?;
    serde_json::to_writer(&mut file, &record).map_err(|source| crate::Error::Json {
        path: path.to_owned(),
        source,
    })?;
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
