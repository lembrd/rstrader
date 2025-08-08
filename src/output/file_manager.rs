use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{AppError, Result};
use crate::types::ExchangeId;

/// Manages file naming and sequencing for multi-stream parquet output
pub struct FileManager {
    output_directory: PathBuf,
    sequence_counters: HashMap<String, AtomicU64>,
}

impl FileManager {
    /// Create a new file manager with the specified output directory
    pub fn new(output_directory: PathBuf) -> Result<Self> {
        if !output_directory.exists() {
            std::fs::create_dir_all(&output_directory).map_err(|e| {
                AppError::io(format!(
                    "Failed to create output directory {}: {}",
                    output_directory.display(),
                    e
                ))
            })?;
        }

        Ok(Self {
            output_directory,
            sequence_counters: HashMap::new(),
        })
    }

    /// Generate a new file path for the given stream parameters
    /// Format: {stream_type}_{exchange}_{symbol}_{sequence:06}.parquet
    /// Example: L2_BINANCE_FUTURES_BTCUSDT_000001.parquet
    pub fn generate_file_path(
        &mut self,
        stream_type: &str,
        exchange: ExchangeId,
        symbol: &str,
    ) -> PathBuf {
        let key = format!("{}_{}_{}",
            stream_type,
            exchange,
            self.normalize_symbol(exchange, symbol)
        );
        
        let counter = self.sequence_counters
            .entry(key.clone())
            .or_insert_with(|| AtomicU64::new(0));
        
        // Increment counter and get the new value
        let sequence = counter.fetch_add(1, Ordering::SeqCst) + 1;
        
        log::debug!("Generating file path for key '{}': sequence = {}", key, sequence);
        
        let filename = format!(
            "{}_{}_{}_{:06}.parquet",
            stream_type,
            exchange,
            self.normalize_symbol(exchange, symbol),
            sequence
        );
        
        self.output_directory.join(filename)
    }

    /// Get the current sequence number for a stream (without incrementing)
    pub fn current_sequence(&self, stream_type: &str, exchange: ExchangeId, symbol: &str) -> u64 {
        let key = format!("{}_{}_{}",
            stream_type,
            exchange,
            self.normalize_symbol(exchange, symbol)
        );
        self.sequence_counters
            .get(&key)
            .map(|counter| counter.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// List existing files for a specific stream type and symbol
    pub fn list_existing_files(
        &self,
        stream_type: &str,
        exchange: ExchangeId,
        symbol: &str,
    ) -> Result<Vec<PathBuf>> {
        let pattern = format!("{}_{}_{}_*.parquet", 
            stream_type, 
            exchange, 
            self.normalize_symbol(exchange, symbol)
        );
        
        let mut files = Vec::new();
        
        if let Ok(entries) = std::fs::read_dir(&self.output_directory) {
            for entry in entries.flatten() {
                if let Some(filename) = entry.file_name().to_str() {
                    if self.matches_pattern(filename, &pattern) {
                        files.push(entry.path());
                    }
                }
            }
        }
        
        files.sort();
        Ok(files)
    }

    /// Initialize sequence counters based on existing files
    pub fn initialize_sequences_from_existing_files(&mut self) -> Result<()> {
        if let Ok(entries) = std::fs::read_dir(&self.output_directory) {
            for entry in entries.flatten() {
                if let Some(filename) = entry.file_name().to_str() {
                    if filename.ends_with(".parquet") {
                        if let Some((sequence, key)) = self.parse_filename(filename) {
                            log::debug!("Found existing file: {} -> sequence: {}, key: {}", filename, sequence, key);
                            let counter = self.sequence_counters
                                .entry(key.clone())
                                .or_insert_with(|| AtomicU64::new(0));
                            
                            // Set to max of current or found sequence
                            let current = counter.load(Ordering::SeqCst);
                            if sequence > current {
                                counter.store(sequence, Ordering::SeqCst);
                                log::debug!("Updated counter for key '{}' from {} to {}", key, current, sequence);
                            }
                        }
                    }
                }
            }
        }
        log::debug!("Initialized sequence counters: {:?}", 
            self.sequence_counters.iter().map(|(k, v)| (k, v.load(Ordering::SeqCst))).collect::<Vec<_>>());
        Ok(())
    }

    /// Get the output directory path
    pub fn output_directory(&self) -> &Path {
        &self.output_directory
    }

    /// Normalize symbol for use in filenames (remove special characters)
    fn normalize_symbol(&self, exchange: ExchangeId, symbol: &str) -> String {
        // Remove any characters that might be problematic in filenames
        symbol.replace('/', "_").replace('-', "_")
    }

    /// Simple pattern matching for file listing
    fn matches_pattern(&self, filename: &str, pattern: &str) -> bool {
        // Simple contains-based match to handle our *_*.parquet use-case without external deps.
        // Pattern like L2_EXCH_SYMBOL_*.parquet -> must start with prefix and end with .parquet
        if let Some(star_idx) = pattern.find('*') {
            let (prefix, suffix) = pattern.split_at(star_idx);
            let suffix = &suffix[1..];
            filename.starts_with(prefix) && filename.ends_with(suffix)
        } else {
            filename == pattern
        }
    }

    /// Parse filename to extract sequence number and key
    /// Format: {stream_type}_{exchange}_{symbol}_{sequence:06}.parquet
    fn parse_filename(&self, filename: &str) -> Option<(u64, String)> {
        if !filename.ends_with(".parquet") {
            return None;
        }
        
        let name_without_ext = &filename[..filename.len() - 8]; // Remove .parquet
        let parts: Vec<&str> = name_without_ext.split('_').collect();
        
        if parts.len() >= 4 {
            if let Ok(sequence) = parts[parts.len() - 1].parse::<u64>() {
                // Reconstruct the key (everything before sequence)
                let key = parts[..parts.len() - 1].join("_");
                return Some((sequence, key));
            }
        }
        
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_file_manager_creation() {
        let temp_dir = tempdir().unwrap();
        let file_manager = FileManager::new(temp_dir.path().to_path_buf()).unwrap();
        assert_eq!(file_manager.output_directory(), temp_dir.path());
    }

    #[test]
    fn test_generate_file_path() {
        let temp_dir = tempdir().unwrap();
        let mut file_manager = FileManager::new(temp_dir.path().to_path_buf()).unwrap();
        
        let path1 = file_manager.generate_file_path("L2", ExchangeId::BinanceFutures, "BTCUSDT");
        let path2 = file_manager.generate_file_path("L2", ExchangeId::BinanceFutures, "BTCUSDT");
        
        assert!(path1.to_string_lossy().contains("L2_BINANCE_FUTURES_BTCUSDT_000001.parquet"));
        assert!(path2.to_string_lossy().contains("L2_BINANCE_FUTURES_BTCUSDT_000002.parquet"));
    }

    #[test]
    fn test_sequence_tracking() {
        let temp_dir = tempdir().unwrap();
        let mut file_manager = FileManager::new(temp_dir.path().to_path_buf()).unwrap();
        
        assert_eq!(file_manager.current_sequence("L2", ExchangeId::BinanceFutures, "BTCUSDT"), 0);
        
        file_manager.generate_file_path("L2", ExchangeId::BinanceFutures, "BTCUSDT");
        assert_eq!(file_manager.current_sequence("L2", ExchangeId::BinanceFutures, "BTCUSDT"), 1);
        
        file_manager.generate_file_path("L2", ExchangeId::BinanceFutures, "BTCUSDT");
        assert_eq!(file_manager.current_sequence("L2", ExchangeId::BinanceFutures, "BTCUSDT"), 2);
    }

    #[test]
    fn test_different_streams_separate_sequences() {
        let temp_dir = tempdir().unwrap();
        let mut file_manager = FileManager::new(temp_dir.path().to_path_buf()).unwrap();
        
        file_manager.generate_file_path("L2", ExchangeId::BinanceFutures, "BTCUSDT");
        file_manager.generate_file_path("TRADES", ExchangeId::BinanceFutures, "BTCUSDT");
        
        assert_eq!(file_manager.current_sequence("L2", ExchangeId::BinanceFutures, "BTCUSDT"), 1);
        assert_eq!(file_manager.current_sequence("TRADES", ExchangeId::BinanceFutures, "BTCUSDT"), 1);
    }

    #[test]
    fn test_normalize_symbol() {
        let temp_dir = tempdir().unwrap();
        let file_manager = FileManager::new(temp_dir.path().to_path_buf()).unwrap();
        
        assert_eq!(file_manager.normalize_symbol(ExchangeId::BinanceFutures, "BTC/USDT"), "BTC_USDT");
        assert_eq!(file_manager.normalize_symbol(ExchangeId::BinanceFutures, "BTC-USDT"), "BTC_USDT");
        assert_eq!(file_manager.normalize_symbol(ExchangeId::BinanceFutures, "BTCUSDT"), "BTCUSDT");
    }

    #[test]
    fn test_parse_filename() {
        let temp_dir = tempdir().unwrap();
        let file_manager = FileManager::new(temp_dir.path().to_path_buf()).unwrap();
        
        let result = file_manager.parse_filename("L2_BINANCE_FUTURES_BTCUSDT_000001.parquet");
        assert_eq!(result, Some((1, "L2_BINANCE_FUTURES_BTCUSDT".to_string())));
        
        let result = file_manager.parse_filename("TRADES_OKX_SWAP_ETHUSDT_000123.parquet");
        assert_eq!(result, Some((123, "TRADES_OKX_SWAP_ETHUSDT".to_string())));
        
        let result = file_manager.parse_filename("invalid.txt");
        assert_eq!(result, None);
    }
}