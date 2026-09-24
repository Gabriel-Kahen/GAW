mod args;
mod protocol;

use std::{path::Path, process::ExitCode};

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser, error::ErrorKind};
use gaw_audio::{
    ChannelLayout, OfflineWavSpec, WavEncoding, compile_project_store, render_compiled_wav,
};
use gaw_core::{Command as CoreCommand, Transaction};
use gaw_project::{ProjectStore, export_midi, import_midi};
use serde_json::{Value, json};

use args::{
    ApplyArgs, ChannelRule, Cli, Command, CreateArgs, Encoding, ExportArgs, ImportArgs,
    MidiExportArgs, MidiImportArgs, RecoverArgs, SchemaKind, TailRule,
};
use protocol::{print_error, print_json, read_json};

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
        .paged_snapshot([])
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
    let report = render_compiled_wav(
        &compiled,
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
    let schema = match kind {
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
    print_json(&schema)
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
    fn infers_project_name_from_directory() {
        assert_eq!(inferred_project_name(Path::new("music/demo")), "demo");
        assert_eq!(inferred_project_name(Path::new("/")), "Untitled");
    }
}
