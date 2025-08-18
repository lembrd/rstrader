mod cli;
mod exchanges;
mod output;
mod metrics;
mod app;
mod strats;
mod md;
mod xcommons;
mod trading;

use clap::Parser;
use cli::Args;
use crate::xcommons::error::{AppError, Result};
// minimal HTTP server for /metrics via tokio TcpListener to avoid heavy deps
// use std::sync::{Arc, Mutex};



#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if it exists (ignore errors if file doesn't exist)
    if let Err(e) = dotenv::dotenv() {
        // Only log if it's not a "not found" error
        if !e.to_string().contains("not found") {
            log::warn!("Failed to load .env file: {}", e);
        }
    }

    // Initialize TLS crypto provider
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| AppError::fatal("Failed to install TLS crypto provider"))?;

    // Initialize logging: also write to file if LOG_FILE is set
    if let Ok(path) = std::env::var("LOG_FILE") {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path);
        match file {
            Ok(file) => {
                use std::io::Write;
                let env = env_logger::Env::default();
                let mut builder = env_logger::Builder::from_env(env);
                builder.format(move |buf, record| {
                    let ts = buf.timestamp_millis();
                    let line = format!("{} {} [{}] {}\n", ts, record.level(), record.module_path().unwrap_or("unknown"), record.args());
                    let _ = writeln!(buf, "{}", line.trim_end());
                    let _ = writeln!(&file, "{}", line.trim_end());
                    Ok(())
                });
                builder.init();
            }
            Err(_) => {
                env_logger::init();
            }
        }
    } else {
        env_logger::init();
    }

    // Parse command line arguments
    let args = Args::parse();

    // Configuration logging is now handled in each mode separately

    // Initialize and run the application in strategy runtime mode only
    let result = app::runtime::run_with_env(args).await;
    match result {
        Ok(_) => {
            log::info!("Application completed successfully");
            Ok(())
        }
        Err(e) => {
            log::error!("Application failed: {}", e);
            Err(e)
        }
    }
}

// legacy run_application removed