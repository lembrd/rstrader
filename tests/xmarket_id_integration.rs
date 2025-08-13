use xtrader::xcommons::xmarket_id::XMarketId;
use xtrader::xcommons::types::ExchangeId;

// Fetch Binance Futures symbols via /fapi/v1/exchangeInfo
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

// Fetch OKX instruments for SWAP and SPOT
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

#[tokio::test]
async fn test_xmarket_id_uniqueness_and_stability() -> anyhow::Result<()> {
    // Fetch symbol sets
    let (binance, okx_swap, okx_spot) = tokio::try_join!(
        fetch_binance_futures_symbols(),
        fetch_okx_symbols("SWAP"),
        fetch_okx_symbols("SPOT"),
    )?;

    // Build IDs and check uniqueness per exchange
    let check = |exchange: ExchangeId, symbols: &Vec<String>| {
        let mut set = std::collections::HashSet::<i64>::with_capacity(symbols.len());
        for s in symbols {
            let id = XMarketId::make(exchange, s);
            assert!(set.insert(id), "duplicate id for {} {:?}", exchange as u8, s);
            assert_eq!(XMarketId::exchange_from(id), exchange as u8);
        }
    };

    check(ExchangeId::BinanceFutures, &binance);
    check(ExchangeId::OkxSwap, &okx_swap);
    check(ExchangeId::OkxSpot, &okx_spot);

    // Cross-exchange collision check: build combined set and ensure no duplicates across exchanges
    let mut all = std::collections::HashSet::<i64>::new();
    for s in &binance { assert!(all.insert(XMarketId::make(ExchangeId::BinanceFutures, s))); }
    for s in &okx_swap { assert!(all.insert(XMarketId::make(ExchangeId::OkxSwap, s))); }
    for s in &okx_spot { assert!(all.insert(XMarketId::make(ExchangeId::OkxSpot, s))); }

    // Determinism: recompute for a sample
    let sample = [
        (ExchangeId::BinanceFutures, binance.get(0).cloned()),
        (ExchangeId::OkxSwap, okx_swap.get(0).cloned()),
        (ExchangeId::OkxSpot, okx_spot.get(0).cloned()),
    ];
    for (ex, sym_opt) in sample {
        if let Some(sym) = sym_opt {
            let a = XMarketId::make(ex, &sym);
            let b = XMarketId::make(ex, &sym);
            assert_eq!(a, b);
        }
    }

    Ok(())
}


