use xtrader::xcommons::xmarket_id::XMarketId;
use xtrader::xcommons::types::ExchangeId;

async fn fetch_binance_futures_symbols() -> anyhow::Result<Vec<String>> {
    let client = reqwest::Client::new();
    let url = "https://fapi.binance.com/fapi/v1/exchangeInfo";
    let resp = client.get(url).send().await?.error_for_status()?;
    let v: serde_json::Value = resp.json().await?;
    let mut symbols = Vec::new();
    if let Some(arr) = v.get("symbols").and_then(|s| s.as_array()) {
        for s in arr {
            if let Some(sym) = s.get("symbol").and_then(|s| s.as_str()) {
                symbols.push(sym.to_string());
            }
        }
    }
    Ok(symbols)
}

async fn fetch_okx_symbols(inst_type: &str) -> anyhow::Result<Vec<String>> {
    let client = reqwest::Client::new();
    let url = format!("https://www.okx.com/api/v5/public/instruments?instType={}", inst_type);
    let resp = client.get(url).send().await?.error_for_status()?;
    let v: serde_json::Value = resp.json().await?;
    let mut symbols = Vec::new();
    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
        for s in arr {
            if let Some(sym) = s.get("instId").and_then(|s| s.as_str()) {
                symbols.push(sym.to_string());
            }
        }
    }
    Ok(symbols)
}

fn print_examples(exchange: ExchangeId, symbols: &Vec<String>, n: usize) {
    println!("Examples for {:?}:", exchange);
    for s in symbols.iter().take(n) {
        let id = XMarketId::make(exchange, s);
        println!("  {} -> {} (0x{:016X})", s, id, id as u64);
    }
}

fn verify_uniqueness(exchange: ExchangeId, symbols: &Vec<String>) {
    let mut set = std::collections::HashSet::<i64>::with_capacity(symbols.len());
    for s in symbols {
        let id = XMarketId::make(exchange, s);
        assert!(set.insert(id), "collision for {:?} {}", exchange, s);
    }
    println!("Unique IDs for {:?}: {}", exchange, set.len());
}

fn measure_throughput(pairs: &[(ExchangeId, String)], repeats: usize) {
    use std::time::Instant;
    let total_ops = pairs.len() * repeats;
    let start = Instant::now();
    let mut acc: i64 = 0;
    for _ in 0..repeats {
        for (ex, s) in pairs {
            acc ^= XMarketId::make(*ex, s);
        }
    }
    let elapsed = start.elapsed();
    let secs = elapsed.as_secs_f64();
    let throughput = (total_ops as f64) / secs;
    println!(
        "Throughput: {:.2} M ids/s ({} ops in {:.3}s), checksum={}",
        throughput / 1e6,
        total_ops,
        secs,
        acc
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (binance, okx_swap, okx_spot) = tokio::try_join!(
        fetch_binance_futures_symbols(),
        fetch_okx_symbols("SWAP"),
        fetch_okx_symbols("SPOT"),
    )?;

    println!("Fetched counts: binance_futures={} okx_swap={} okx_spot={}", binance.len(), okx_swap.len(), okx_spot.len());

    // Print examples
    print_examples(ExchangeId::BinanceFutures, &binance, 10);
    print_examples(ExchangeId::OkxSwap, &okx_swap, 10);
    print_examples(ExchangeId::OkxSpot, &okx_spot, 10);

    // Verify uniqueness per exchange
    verify_uniqueness(ExchangeId::BinanceFutures, &binance);
    verify_uniqueness(ExchangeId::OkxSwap, &okx_swap);
    verify_uniqueness(ExchangeId::OkxSpot, &okx_spot);

    // Verify uniqueness across exchanges (guaranteed by partitioning)
    let mut all = std::collections::HashSet::<i64>::new();
    for s in &binance { assert!(all.insert(XMarketId::make(ExchangeId::BinanceFutures, s))); }
    for s in &okx_swap { assert!(all.insert(XMarketId::make(ExchangeId::OkxSwap, s))); }
    for s in &okx_spot { assert!(all.insert(XMarketId::make(ExchangeId::OkxSpot, s))); }
    println!("Unique across all exchanges: {}", all.len());

    // Measure throughput in release build
    let mut pairs: Vec<(ExchangeId, String)> = Vec::new();
    pairs.extend(binance.into_iter().map(|s| (ExchangeId::BinanceFutures, s)));
    pairs.extend(okx_swap.into_iter().map(|s| (ExchangeId::OkxSwap, s)));
    pairs.extend(okx_spot.into_iter().map(|s| (ExchangeId::OkxSpot, s)));

    // Warmup
    measure_throughput(&pairs, 1);
    // Measure
    measure_throughput(&pairs, 50);

    Ok(())
}


