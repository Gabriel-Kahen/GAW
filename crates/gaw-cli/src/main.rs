use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "gaw", version, about = "Agent-native audio workstation CLI")]
struct Cli {}

fn main() {
    let _ = Cli::parse();
}
