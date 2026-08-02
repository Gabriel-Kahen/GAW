use std::{
    fs::File,
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind};
use gaw_audio::{ChannelLayout, OfflineWavSpec, WavEncoding, compile_project_store, render_wav};
use gaw_core::{
    Command as CoreCommand, EventDataId, ParameterDescriptor, ParameterValueType, ProcessorKind,
    Transaction,
};
use gaw_project::{ProjectStore, export_midi, import_midi};
use serde::Serialize;
use serde_json::{Value, json};

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
    /// Deterministically render the root composition to a WAV file.
    Export(ExportArgs),
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
struct ExportArgs {
    /// Project directory.
    project: PathBuf,

    /// Destination `.wav` file.
    destination: PathBuf,

    /// Output sample rate. Defaults to the project's internal sample rate.
    #[arg(long, value_parser = positive_u32)]
    sample_rate: Option<u32>,

    /// Explicit output channel conversion rule.
    #[arg(long, value_enum, default_value_t = ChannelRule::Native)]
    channels: ChannelRule,

    /// First source frame at the project's internal sample rate.
    #[arg(long, default_value_t = 0)]
    start_frame: u64,

    /// Exact source-frame count. Omit to render through the selected range end.
    #[arg(long, value_parser = positive_u64)]
    frames: Option<u64>,

    /// Whether the valid render range includes the finite declared tail.
    #[arg(long, value_enum, default_value_t = TailRule::Include)]
    tail: TailRule,

    /// WAV sample encoding.
    #[arg(long, value_enum, default_value_t = Encoding::Float32)]
    encoding: Encoding,

    /// Bounded offline working block size. Does not change sample values.
    #[arg(long, default_value_t = 4_096, value_parser = positive_usize)]
    block_frames: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum ChannelRule {
    #[default]
    Native,
    Mono,
    Stereo,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum TailRule {
    #[default]
    Include,
    Exclude,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum Encoding {
    #[default]
    Float32,
    Pcm16,
    Pcm24,
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
    Cli,
    Project,
    Command,
    Transaction,
    Processor,
    AnalyzerMeasurement,
    SamplerPreset,
    EffectPreset,
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

fn positive_u64(value: &str) -> std::result::Result<u64, String> {
    let value = value
        .parse::<u64>()
        .map_err(|error| format!("invalid integer: {error}"))?;
    if value == 0 {
        Err("value must be greater than zero".into())
    } else {
        Ok(value)
    }
}

fn positive_usize(value: &str) -> std::result::Result<usize, String> {
    let value = value
        .parse::<usize>()
        .map_err(|error| format!("invalid integer: {error}"))?;
    if value == 0 {
        Err("value must be greater than zero".into())
    } else {
        Ok(value)
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            print_error("cli.invalid_arguments", &anyhow!(error.to_string()));
            return ExitCode::from(2);
        }
    };
    let error_code = cli.command.error_code();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            print_error(error_code, &error);
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Create(args) => create(args),
        Command::Inspect(args) => inspect(&args.project),
        Command::Validate(args) => validate(&args.project),
        Command::Import(args) => import(&args),
        Command::MidiImport(args) => midi_import(&args),
        Command::MidiExport(args) => midi_export(&args),
        Command::Export(args) => export(&args),
        Command::Apply(args) => apply(&args),
        Command::Recover(args) => recover(&args),
        Command::Schema(args) => schema(args.kind),
    }
}

impl Command {
    const fn error_code(&self) -> &'static str {
        match self {
            Self::Create(_) => "project.create_failed",
            Self::Inspect(_) => "project.inspect_failed",
            Self::Validate(_) => "project.validation_failed",
            Self::Import(_) => "asset.import_failed",
            Self::MidiImport(_) => "midi.import_failed",
            Self::MidiExport(_) => "midi.export_failed",
            Self::Export(_) => "audio.export_failed",
            Self::Apply(_) => "transaction.apply_failed",
            Self::Recover(_) => "project.recovery_failed",
            Self::Schema(_) => "schema.discovery_failed",
        }
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

fn export(args: &ExportArgs) -> Result<()> {
    let store = open(&args.project)?;
    let compiled = compile_project_store(&store).context("could not compile project audio")?;
    let snapshot = compiled
        .snapshot()
        .context("could not prepare project audio")?;
    let range_end = match args.tail {
        TailRule::Include => snapshot.total_frames(),
        TailRule::Exclude => snapshot.main_frames(),
    };
    if args.start_frame > range_end {
        bail!(
            "requested start frame {} is past the selected range end {range_end}",
            args.start_frame
        );
    }
    let available_frames = range_end - args.start_frame;
    let source_frames = args.frames.unwrap_or(available_frames);
    if source_frames > available_frames {
        bail!(
            "requested {source_frames} frames from frame {} exceeds the selected range by {} frames",
            args.start_frame,
            source_frames - available_frames
        );
    }
    let layout = match args.channels {
        ChannelRule::Native => snapshot.layout(),
        ChannelRule::Mono => ChannelLayout::Mono,
        ChannelRule::Stereo => ChannelLayout::Stereo,
    };
    let encoding = match args.encoding {
        Encoding::Float32 => WavEncoding::Float32,
        Encoding::Pcm16 => WavEncoding::Pcm16,
        Encoding::Pcm24 => WavEncoding::Pcm24,
    };
    let output_sample_rate = args.sample_rate.unwrap_or_else(|| snapshot.sample_rate());
    let report = render_wav(
        &snapshot,
        &args.destination,
        OfflineWavSpec {
            start_frame: args.start_frame,
            frames: Some(source_frames),
            sample_rate: Some(output_sample_rate),
            layout,
            block_frames: args.block_frames,
            encoding,
        },
    )
    .with_context(|| format!("could not render WAV to {}", args.destination.display()))?;
    print_json(&json!({
        "kind": "gaw.final_export",
        "schema_version": 1,
        "project": args.project,
        "destination": args.destination,
        "revision": snapshot.revision(),
        "source": {
            "sample_rate": snapshot.sample_rate(),
            "layout": layout_name(snapshot.layout()),
            "start_frame": args.start_frame,
            "frames": source_frames,
            "main_frames": snapshot.main_frames(),
            "tail_frames": snapshot.tail_frames(),
            "tail_included": args.tail == TailRule::Include,
        },
        "output": {
            "sample_rate": report.sample_rate,
            "layout": layout_name(report.layout),
            "frames": report.frames,
            "encoding": encoding_name(args.encoding),
        }
    }))
}

fn apply(args: &ApplyArgs) -> Result<()> {
    let transaction: Transaction = read_json(&args.transaction)?;
    let store = open(&args.project)?;
    print_json(&store.commit_transaction(&transaction)?)
}

fn schema(kind: SchemaKind) -> Result<()> {
    let includes_processors = matches!(
        kind,
        SchemaKind::Project
            | SchemaKind::Command
            | SchemaKind::Transaction
            | SchemaKind::Processor
            | SchemaKind::EffectPreset
    );
    let mut schema = match kind {
        SchemaKind::Cli => cli_schema(),
        SchemaKind::Project => serde_json::to_value(gaw_core::project_json_schema())?,
        SchemaKind::Command => serde_json::to_value(gaw_core::command_json_schema())?,
        SchemaKind::Transaction => serde_json::to_value(gaw_core::transaction_json_schema())?,
        SchemaKind::Processor => serde_json::to_value(gaw_core::processor_json_schema())?,
        SchemaKind::AnalyzerMeasurement => {
            serde_json::to_value(gaw_core::analyzer_measurement_json_schema())?
        }
        SchemaKind::SamplerPreset => serde_json::to_value(gaw_core::sampler_preset_json_schema())?,
        SchemaKind::EffectPreset => serde_json::to_value(gaw_core::effect_preset_json_schema())?,
    };
    if includes_processors {
        schema
            .as_object_mut()
            .context("schema root is not an object")?
            .insert("x-gaw-processor-catalog".into(), processor_catalog()?);
    }
    print_json(&schema)
}

fn processor_catalog() -> Result<Value> {
    let processors = ProcessorKind::catalog_defaults()
        .iter()
        .map(processor_catalog_entry)
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "schema_version": 1,
        "description": "Authoritative canonical-validation contract for processor parameters. Numeric bounds are inclusive; unit_ranges apply to tagged time/rate values; constraints apply after per-parameter validation.",
        "processors": processors,
    }))
}

fn processor_catalog_entry(kind: &ProcessorKind) -> Result<Value> {
    let parameters = kind
        .parameter_descriptors()
        .iter()
        .map(|descriptor| processor_parameter(kind, descriptor))
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "type": kind.type_id(),
        "analyzer": kind.is_analyzer(),
        "parameters": parameters,
        "constraints": processor_constraints(kind),
    }))
}

fn processor_parameter(kind: &ProcessorKind, descriptor: &ParameterDescriptor) -> Result<Value> {
    let mut parameter = serde_json::Map::from_iter([
        ("id".into(), Value::from(descriptor.id)),
        (
            "value_type".into(),
            serde_json::to_value(descriptor.value_type)?,
        ),
        ("unit".into(), serde_json::to_value(descriptor.unit)?),
        (
            "default".into(),
            serde_json::from_str(descriptor.default_json)
                .with_context(|| format!("invalid catalog default for {}", descriptor.id))?,
        ),
        (
            "automation".into(),
            serde_json::to_value(descriptor.automation)?,
        ),
        (
            "display_hint".into(),
            serde_json::to_value(descriptor.display_hint)?,
        ),
    ]);

    match descriptor.value_type {
        ParameterValueType::Number | ParameterValueType::Integer => {
            let range = descriptor
                .range
                .context("numeric catalog parameter is missing its range")?;
            parameter.insert("minimum".into(), Value::from(range.minimum));
            parameter.insert("maximum".into(), Value::from(range.maximum));
            if matches!(kind, ProcessorKind::BeatRepeat(_)) && descriptor.id == "seed" {
                parameter.insert("maximum".into(), Value::from(u64::MAX));
            }
        }
        ParameterValueType::Choice => {
            parameter.insert("enum".into(), serde_json::to_value(descriptor.choices)?);
        }
        ParameterValueType::Time => {
            let minimum = if matches!(kind, ProcessorKind::Delay(_)) && descriptor.id == "time" {
                f64::EPSILON
            } else {
                0.0
            };
            parameter.insert(
                "unit_ranges".into(),
                json!({
                    "beats": { "minimum": minimum, "maximum": 64.0 },
                    "seconds": { "minimum": minimum, "maximum": 64.0 },
                }),
            );
        }
        ParameterValueType::Rate => {
            parameter.insert(
                "unit_ranges".into(),
                json!({
                    "hertz": { "minimum": 0.01, "maximum": 40.0 },
                    "beats": { "minimum": 1.0 / 64.0, "maximum": 64.0 },
                }),
            );
        }
        ParameterValueType::List => match kind {
            ProcessorKind::ParametricEq(_) if descriptor.id == "bands" => {
                parameter.insert("minItems".into(), Value::from(0));
                parameter.insert("maxItems".into(), Value::from(8));
            }
            ProcessorKind::RhythmicGate(_) if descriptor.id == "steps" => {
                parameter.insert("minItems".into(), Value::from(1));
                parameter.insert("maxItems".into(), Value::from(64));
            }
            _ => {}
        },
        ParameterValueType::Boolean => {}
    }
    Ok(Value::Object(parameter))
}

fn processor_constraints(kind: &ProcessorKind) -> Value {
    let ordered = |lower, upper| {
        json!([{
            "kind": "less_than",
            "lower": lower,
            "upper": upper,
        }])
    };
    match kind {
        ProcessorKind::Delay(_) | ProcessorKind::Reverb(_) => ordered("low_cut_hz", "high_cut_hz"),
        ProcessorKind::Spectrum(_) | ProcessorKind::Tuner(_) => ordered("minimum_hz", "maximum_hz"),
        ProcessorKind::Gain(_)
        | ProcessorKind::StereoTool(_)
        | ProcessorKind::Filter(_)
        | ProcessorKind::ParametricEq(_)
        | ProcessorKind::Compressor(_)
        | ProcessorKind::Limiter(_)
        | ProcessorKind::Gate(_)
        | ProcessorKind::Expander(_)
        | ProcessorKind::TransientShaper(_)
        | ProcessorKind::Saturator(_)
        | ProcessorKind::Clipper(_)
        | ProcessorKind::Bitcrusher(_)
        | ProcessorKind::Chorus(_)
        | ProcessorKind::Flanger(_)
        | ProcessorKind::Phaser(_)
        | ProcessorKind::TremoloAutopan(_)
        | ProcessorKind::PitchShift(_)
        | ProcessorKind::RhythmicGate(_)
        | ProcessorKind::BeatRepeat(_)
        | ProcessorKind::LevelMeter(_)
        | ProcessorKind::LoudnessMeter(_)
        | ProcessorKind::Oscilloscope(_)
        | ProcessorKind::StereoMeter(_) => json!([]),
    }
}

fn cli_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "GAW CLI protocol",
        "type": "object",
        "description": "Successful commands write one JSON value to stdout. Runtime and argument failures write one GAW CLI error object to stderr.",
        "$defs": {
            "Error": {
                "type": "object",
                "additionalProperties": false,
                "required": ["kind", "schema_version", "code", "message", "causes"],
                "properties": {
                    "kind": { "const": "gaw.error" },
                    "schema_version": { "const": 1 },
                    "code": { "type": "string" },
                    "message": { "type": "string" },
                    "causes": { "type": "array", "items": { "type": "string" } }
                }
            },
            "FinalExport": {
                "type": "object",
                "required": ["kind", "schema_version", "project", "destination", "revision", "source", "output"],
                "properties": {
                    "kind": { "const": "gaw.final_export" },
                    "schema_version": { "const": 1 },
                    "project": { "type": "string" },
                    "destination": { "type": "string" },
                    "revision": { "type": "integer", "minimum": 0 },
                    "source": { "type": "object" },
                    "output": { "type": "object" }
                }
            }
        },
        "commands": Cli::command().get_subcommands().map(clap::Command::get_name).collect::<Vec<_>>()
    })
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

#[derive(Debug, Serialize)]
struct ErrorOutput<'a> {
    kind: &'static str,
    schema_version: u32,
    code: &'a str,
    message: String,
    causes: Vec<String>,
}

fn print_error(code: &str, error: &anyhow::Error) {
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
    eprintln!();
}

const fn layout_name(layout: ChannelLayout) -> &'static str {
    match layout {
        ChannelLayout::Mono => "mono",
        ChannelLayout::Stereo => "stereo",
    }
}

const fn encoding_name(encoding: Encoding) -> &'static str {
    match encoding {
        Encoding::Float32 => "float32",
        Encoding::Pcm16 => "pcm16",
        Encoding::Pcm24 => "pcm24",
    }
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
            vec!["gaw", "export", "demo", "mix.wav"],
            vec!["gaw", "apply", "demo", "-"],
            vec!["gaw", "recover", "demo", "--dry-run"],
            vec!["gaw", "schema", "transaction"],
            vec!["gaw", "schema", "cli"],
            vec!["gaw", "schema", "sampler-preset"],
            vec!["gaw", "schema", "effect-preset"],
        ] {
            Cli::try_parse_from(args).unwrap();
        }
    }

    #[test]
    fn rejects_invalid_creation_quantities() {
        assert!(Cli::try_parse_from(["gaw", "create", "demo", "--bpm", "NaN"]).is_err());
        assert!(Cli::try_parse_from(["gaw", "create", "demo", "--sample-rate", "0"]).is_err());
        assert!(
            Cli::try_parse_from(["gaw", "export", "demo", "mix.wav", "--frames", "0"]).is_err()
        );
    }

    #[test]
    fn infers_project_name_from_directory() {
        assert_eq!(inferred_project_name(Path::new("music/demo")), "demo");
        assert_eq!(inferred_project_name(Path::new("/")), "Untitled");
    }
}
