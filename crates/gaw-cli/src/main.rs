use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use gaw_core::{Command as CoreCommand, EventDataId, Transaction};
use gaw_project::{ProjectStore, export_midi, import_midi};
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
    /// Convert a Standard MIDI File into canonical event streams.
    MidiImport(MidiImportArgs),
    /// Export one canonical event stream as a Standard MIDI File.
    MidiExport(MidiExportArgs),
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
struct MidiImportArgs {
    /// Project directory.
    project: PathBuf,

    /// Standard MIDI File to convert. The MIDI file is not copied into the project.
    source: PathBuf,
}

#[derive(Debug, Args)]
struct MidiExportArgs {
    /// Project directory.
    project: PathBuf,

    /// Stable ID of the canonical event stream to export.
    event_data_id: EventDataId,

    /// Destination `.mid` file.
    destination: PathBuf,

    /// Pulses per quarter note in the exported file.
    #[arg(long, default_value_t = 960, value_parser = clap::value_parser!(u16).range(1..=32_767))]
    ppqn: u16,
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
        Command::MidiImport(args) => midi_import(&args),
        Command::MidiExport(args) => midi_export(&args),
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

fn midi_import(args: &MidiImportArgs) -> Result<()> {
    let imported = import_midi(&args.source)
        .with_context(|| format!("could not import MIDI from {}", args.source.display()))?;
    let transaction = Transaction::named(
        format!("Import MIDI {}", args.source.display()),
        imported
            .event_data
            .iter()
            .cloned()
            .map(|event_data| CoreCommand::AddEventData { event_data }),
    );
    open(&args.project)?.commit_transaction(&transaction)?;
    print_json(&imported)
}

fn midi_export(args: &MidiExportArgs) -> Result<()> {
    let project = open(&args.project)?.load_project()?;
    let event_data = project
        .event_data
        .iter()
        .find(|value| value.id == args.event_data_id)
        .with_context(|| format!("event data {} does not exist", args.event_data_id))?;
    export_midi(event_data, project.bpm, args.ppqn, &args.destination)
        .with_context(|| format!("could not export MIDI to {}", args.destination.display()))?;
    print_json(&json!({
        "event_data_id": args.event_data_id,
        "destination": args.destination,
        "ppqn": args.ppqn,
    }))
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
            vec!["gaw", "midi-import", "demo", "notes.mid"],
            vec![
                "gaw",
                "midi-export",
                "demo",
                "00000000-0000-0000-0000-000000000001",
                "notes.mid",
            ],
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
