use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use gaw_core::Transaction;
use gaw_project::ProjectStore;
use serde_json::json;

#[derive(Debug, Parser)]
#[command(name = "gaw", version, about = "Agent-native audio workstation CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a new project directory.
    Create(CreateArgs),
    /// Print a project's complete canonical snapshot as JSON.
    Inspect(ProjectArgs),
    /// Validate all canonical project documents.
    Validate(ProjectArgs),
    /// Copy an immutable media file into a project's asset store.
    Import(ImportArgs),
    /// Apply one atomic JSON transaction from a file or standard input.
    Apply(ApplyArgs),
    /// Replay transactions left by an interrupted write.
    Recover(RecoverArgs),
    /// Print a canonical JSON Schema for agent discovery.
    Schema(SchemaArgs),
}

#[derive(Debug, Args)]
struct ProjectArgs {
    /// Project directory.
    project: PathBuf,
}

#[derive(Debug, Args)]
struct CreateArgs {
    /// Directory to create as a GAW project.
    project: PathBuf,

    /// Human-readable project name.
    #[arg(long)]
    name: Option<String>,

    /// Project tempo in beats per minute.
    #[arg(long, default_value_t = 120.0, value_parser = positive_f64)]
    bpm: f64,

    /// Internal project sample rate in frames per second.
    #[arg(long, default_value_t = 48_000, value_parser = positive_u32)]
    sample_rate: u32,
}

#[derive(Debug, Args)]
struct ImportArgs {
    /// Project directory.
    project: PathBuf,

    /// Audio file to import.
    source: PathBuf,
}

#[derive(Debug, Args)]
struct ApplyArgs {
    /// Project directory.
    project: PathBuf,

    /// Transaction JSON file, or '-' to read standard input.
    transaction: PathBuf,
}

#[derive(Debug, Args)]
struct RecoverArgs {
    /// Project directory.
    project: PathBuf,

    /// Inspect pending recovery records without replaying them.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct SchemaArgs {
    #[arg(value_enum)]
    kind: SchemaKind,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SchemaKind {
    Project,
    Command,
    Transaction,
    Processor,
    AnalyzerMeasurement,
}

fn positive_f64(value: &str) -> std::result::Result<f64, String> {
    let value = value
        .parse::<f64>()
        .map_err(|error| format!("invalid number: {error}"))?;
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err("value must be finite and greater than zero".into())
    }
}

fn positive_u32(value: &str) -> std::result::Result<u32, String> {
    let value = value
        .parse::<u32>()
        .map_err(|error| format!("invalid integer: {error}"))?;
    if value == 0 {
        Err("value must be greater than zero".into())
    } else {
        Ok(value)
    }
}

fn main() -> Result<()> {
    run(Cli::parse())
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Create(args) => create(args),
        Command::Inspect(args) => inspect(&args.project),
        Command::Validate(args) => validate(&args.project),
        Command::Import(args) => import(&args),
        Command::Apply(args) => apply(&args),
        Command::Recover(args) => recover(&args),
        Command::Schema(args) => schema(args.kind),
    }
}

fn create(args: CreateArgs) -> Result<()> {
    let name = args
        .name
        .unwrap_or_else(|| inferred_project_name(&args.project));
    let store = ProjectStore::create_default(&args.project, &name, args.bpm, args.sample_rate)
        .with_context(|| format!("could not create project at {}", args.project.display()))?;
    print_json(&store.load_project()?)
}

fn inspect(project: &Path) -> Result<()> {
    let store = open(project)?;
    print_json(&store.load_project()?)
}

fn validate(project: &Path) -> Result<()> {
    let report = ProjectStore::validate_path(project)?;
    print_json(&report)?;
    if report.is_valid() {
        Ok(())
    } else {
        bail!("project validation failed")
    }
}

fn import(args: &ImportArgs) -> Result<()> {
    let store = open(&args.project)?;
    let imported = store
        .import_media(&args.source)
        .with_context(|| format!("could not import {}", args.source.display()))?;
    print_json(&imported)
}

fn apply(args: &ApplyArgs) -> Result<()> {
    let transaction: Transaction = read_json(&args.transaction)?;
    let store = open(&args.project)?;
    print_json(&store.commit_transaction(&transaction)?)
}

fn schema(kind: SchemaKind) -> Result<()> {
    let schema = match kind {
        SchemaKind::Project => gaw_core::project_json_schema(),
        SchemaKind::Command => gaw_core::command_json_schema(),
        SchemaKind::Transaction => gaw_core::transaction_json_schema(),
        SchemaKind::Processor => gaw_core::processor_json_schema(),
        SchemaKind::AnalyzerMeasurement => gaw_core::analyzer_measurement_json_schema(),
    };
    print_json(&schema)
}

fn recover(args: &RecoverArgs) -> Result<()> {
    let store = open(&args.project)?;
    if args.dry_run {
        return print_json(&store.pending_recovery()?);
    }

    let recovered_transactions = store.recover()?;
    print_json(&json!({ "recovered_transactions": recovered_transactions }))
}

fn open(project: &Path) -> Result<ProjectStore> {
    ProjectStore::open(project)
        .with_context(|| format!("could not open project at {}", project.display()))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    if path == Path::new("-") {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .context("could not read transaction JSON from standard input")?;
        serde_json::from_str(&input).context("invalid transaction JSON from standard input")
    } else {
        let file = File::open(path)
            .with_context(|| format!("could not open transaction file {}", path.display()))?;
        serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("invalid transaction JSON in {}", path.display()))
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    serde_json::to_writer_pretty(io::stdout().lock(), value)?;
    println!();
    Ok(())
}

fn inferred_project_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("Untitled")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_command() {
        for args in [
            vec!["gaw", "create", "demo"],
            vec!["gaw", "inspect", "demo"],
            vec!["gaw", "validate", "demo"],
            vec!["gaw", "import", "demo", "kick.wav"],
            vec!["gaw", "apply", "demo", "-"],
            vec!["gaw", "recover", "demo", "--dry-run"],
            vec!["gaw", "schema", "transaction"],
        ] {
            Cli::try_parse_from(args).unwrap();
        }
    }

    #[test]
    fn rejects_invalid_creation_quantities() {
        assert!(Cli::try_parse_from(["gaw", "create", "demo", "--bpm", "NaN"]).is_err());
        assert!(Cli::try_parse_from(["gaw", "create", "demo", "--sample-rate", "0"]).is_err());
    }

    #[test]
    fn infers_project_name_from_directory() {
        assert_eq!(inferred_project_name(Path::new("music/demo")), "demo");
        assert_eq!(inferred_project_name(Path::new("/")), "Untitled");
    }
}
