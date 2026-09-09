use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use gaw_core::EventDataId;

#[derive(Debug, Parser)]
#[command(name = "gaw", version, about = "Agent-native audio workstation CLI")]
pub(super) struct Cli {
    #[command(subcommand)]
    pub(super) command: Command,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
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
pub(super) struct ProjectArgs {
    /// Project directory.
    pub(super) project: PathBuf,
}

#[derive(Debug, Args)]
pub(super) struct CreateArgs {
    /// Directory to create as a GAW project.
    pub(super) project: PathBuf,

    /// Human-readable project name.
    #[arg(long)]
    pub(super) name: Option<String>,

    /// Project tempo in beats per minute.
    #[arg(long, default_value_t = 120.0, value_parser = positive_f64)]
    pub(super) bpm: f64,

    /// Internal project sample rate in frames per second.
    #[arg(long, default_value_t = 48_000, value_parser = positive_integer::<u32>)]
    pub(super) sample_rate: u32,
}

#[derive(Debug, Args)]
pub(super) struct ImportArgs {
    /// Project directory.
    pub(super) project: PathBuf,

    /// Audio file to decode and import as canonical WAV.
    pub(super) source: PathBuf,
}

#[derive(Debug, Args)]
pub(super) struct MidiImportArgs {
    /// Project directory.
    pub(super) project: PathBuf,

    /// Standard MIDI File to convert. The MIDI file is not copied into the project.
    pub(super) source: PathBuf,
}

#[derive(Debug, Args)]
pub(super) struct MidiExportArgs {
    /// Project directory.
    pub(super) project: PathBuf,

    /// Stable ID of the canonical event stream to export.
    pub(super) event_data_id: EventDataId,

    /// Destination `.mid` file.
    pub(super) destination: PathBuf,

    /// Pulses per quarter note in the exported file.
    #[arg(long, default_value_t = 960, value_parser = clap::value_parser!(u16).range(1..=32_767))]
    pub(super) ppqn: u16,
}

#[derive(Debug, Args)]
pub(super) struct ExportArgs {
    /// Project directory.
    pub(super) project: PathBuf,

    /// Destination `.wav` file.
    pub(super) destination: PathBuf,

    /// Output sample rate. Defaults to the project's internal sample rate.
    #[arg(long, value_parser = positive_integer::<u32>)]
    pub(super) sample_rate: Option<u32>,

    /// Explicit output channel conversion rule.
    #[arg(long, value_enum, default_value_t = ChannelRule::Native)]
    pub(super) channels: ChannelRule,

    /// First source frame at the project's internal sample rate.
    #[arg(long, default_value_t = 0)]
    pub(super) start_frame: u64,

    /// Exact source-frame count. Omit to render through the selected range end.
    #[arg(long, value_parser = positive_integer::<u64>)]
    pub(super) frames: Option<u64>,

    /// Whether the valid render range includes the finite declared tail.
    #[arg(long, value_enum, default_value_t = TailRule::Include)]
    pub(super) tail: TailRule,

    /// WAV sample encoding.
    #[arg(long, value_enum, default_value_t = Encoding::Float32)]
    pub(super) encoding: Encoding,

    /// Bounded offline working block size. Does not change sample values.
    #[arg(long, default_value_t = 4_096, value_parser = positive_integer::<usize>)]
    pub(super) block_frames: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(super) enum ChannelRule {
    #[default]
    Native,
    Mono,
    Stereo,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(super) enum TailRule {
    #[default]
    Include,
    Exclude,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(super) enum Encoding {
    #[default]
    Float32,
    Pcm16,
    Pcm24,
}

#[derive(Debug, Args)]
pub(super) struct ApplyArgs {
    /// Project directory.
    pub(super) project: PathBuf,

    /// Transaction JSON file, or '-' to read standard input.
    pub(super) transaction: PathBuf,
}

#[derive(Debug, Args)]
pub(super) struct RecoverArgs {
    /// Project directory.
    pub(super) project: PathBuf,

    /// Inspect pending recovery records without replaying them.
    #[arg(long)]
    pub(super) dry_run: bool,
}

#[derive(Debug, Args)]
pub(super) struct SchemaArgs {
    #[arg(value_enum)]
    pub(super) kind: SchemaKind,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(super) enum SchemaKind {
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

fn positive_integer<T>(value: &str) -> std::result::Result<T, String>
where
    T: std::str::FromStr + Default + PartialEq,
    T::Err: std::fmt::Display,
{
    let value = value
        .parse::<T>()
        .map_err(|error| format!("invalid integer: {error}"))?;
    if value == T::default() {
        Err("value must be greater than zero".into())
    } else {
        Ok(value)
    }
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
}
