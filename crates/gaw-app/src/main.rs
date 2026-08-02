use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use gaw_app::{GawApp, NativeStartup, RecoveryPolicy};

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum RecoveryArg {
    #[default]
    Recover,
    Discard,
    Abort,
}

impl From<RecoveryArg> for RecoveryPolicy {
    fn from(value: RecoveryArg) -> Self {
        match value {
            RecoveryArg::Recover => Self::Recover,
            RecoveryArg::Discard => Self::Discard,
            RecoveryArg::Abort => Self::Abort,
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "GAW native audio workstation")]
struct Args {
    /// Canonical GAW project directory.
    #[arg(value_name = "PROJECT_DIR", conflicts_with = "demo")]
    project: Option<PathBuf>,
    /// Open the bundled non-persistent UI fixture.
    #[arg(long)]
    demo: bool,
    /// Recovery-journal policy used when opening a project.
    #[arg(long, value_enum, default_value_t)]
    recovery: RecoveryArg,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gaw=info".into()),
        )
        .init();
    let args = Args::parse();
    if !args.demo && args.project.is_none() {
        anyhow::bail!("pass a project directory or use --demo");
    }
    let startup = args
        .project
        .map(|root| NativeStartup::open(root, args.recovery.into()))
        .transpose()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1480.0, 900.0])
            .with_min_inner_size([980.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "GAW",
        options,
        Box::new(move |context| {
            if let Some(startup) = startup {
                Ok(Box::new(GawApp::with_native_project(context, startup)?))
            } else {
                Ok(Box::new(GawApp::new(context)))
            }
        }),
    )?;
    Ok(())
}
