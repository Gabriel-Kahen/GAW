use std::{
    fs::File,
    io::{self, BufReader, Write},
    path::Path,
};

use anyhow::{Context, Result};
use serde::Serialize;

pub(super) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    if path == Path::new("-") {
        serde_json::from_reader(io::stdin().lock())
            .context("invalid transaction JSON from standard input")
    } else {
        let file = File::open(path)
            .with_context(|| format!("could not open transaction file {}", path.display()))?;
        serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("invalid transaction JSON in {}", path.display()))
    }
}

pub(super) fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

#[derive(Debug, Serialize)]
struct ErrorOutput<'a> {
    kind: &'static str,
    schema_version: u32,
    code: &'a str,
    message: String,
    causes: Vec<String>,
}

pub(super) fn print_error(code: &str, error: &anyhow::Error) {
    let causes = error.chain().skip(1).map(ToString::to_string).collect();
    let output = ErrorOutput {
        kind: "gaw.error",
        schema_version: 1,
        code,
        message: error.to_string(),
        causes,
    };
    let mut stderr = io::stderr().lock();
    let _ = serde_json::to_writer_pretty(&mut stderr, &output);
    let _ = writeln!(stderr);
}
