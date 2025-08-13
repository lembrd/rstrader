use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(ValueEnum, Clone, Debug, PartialEq, Eq)]
pub enum Sink { Parquet, QuestDb }

#[derive(Parser, Debug)]
#[command(name = "xtrader")]
#[command(about = "High-performance cryptocurrency market data collector")]
pub struct Args {
    #[arg(long)]
    pub subscriptions: Option<String>,
    #[arg(long, default_value = "./test_output")]
    pub output_directory: PathBuf,
    #[arg(long)]
    pub verbose: bool,
    #[arg(long)]
    pub shutdown_after: Option<u64>,
    #[arg(long, value_enum, default_value_t = Sink::Parquet)]
    pub sink: Sink,
    #[arg(long)]
    pub strategy: Option<String>,
    #[arg(long)]
    pub config: Option<PathBuf>,
}
