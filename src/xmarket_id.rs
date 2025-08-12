use xxhash_rust::xxh3::xxh3_64_with_seed;
use crate::types::ExchangeId;

/// XMarketId provides stable 64-bit identifiers for (exchange, symbol)
/// using a composite scheme: high 8 bits = exchange id, low 56 bits = XXH3 hash of normalized symbol.
/// This avoids requiring a persistent mapping table while keeping collisions negligible for practical sets.
pub struct XMarketId;

const XMARKET_SEED: u64 = 0xA3F0_D1C2_B3E4_A5F6;

impl XMarketId {
    #[inline]
    fn normalize_symbol(symbol: &str) -> Vec<u8> {
        // Uppercase ASCII fast-path; preserve bytes otherwise. Trim spaces.
        let trimmed = symbol.trim();
        let mut out = Vec::with_capacity(trimmed.len());
        for &b in trimmed.as_bytes() {
            let upper = if (b'a'..=b'z').contains(&b) { b - 32 } else { b };
            out.push(upper);
        }
        out
    }

    /// Build 64-bit id: [exchange:8][symbol_hash:56]
    #[inline]
    pub fn make(exchange: ExchangeId, symbol: &str) -> i64 {
        let norm = Self::normalize_symbol(symbol);
        let h = xxh3_64_with_seed(&norm, XMARKET_SEED);
        let low56 = h & ((1u64 << 56) - 1);
        let id = ((exchange as u64) << 56) | low56;
        id as i64
    }

    /// Extract the 8-bit exchange id portion (as u8)
    #[inline]
    pub fn exchange_from(id: i64) -> u8 {
        ((id as u64) >> 56) as u8
    }
}


