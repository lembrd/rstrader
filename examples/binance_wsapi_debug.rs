use std::time::Duration;

use base64::{engine::general_purpose, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use ed25519_dalek::pkcs8::DecodePrivateKey;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::{connect_async, tungstenite::Message};

type E = Box<dyn std::error::Error + Send + Sync + 'static>;

fn read_pkcs8_pem(path: &str) -> Result<SigningKey, E> {
    let pem = std::fs::read_to_string(path)?;
    let trimmed = pem.trim_start();
    if trimmed.starts_with("-----BEGIN") {
        let mut b64 = String::new();
        for line in pem.lines() {
            let l = line.trim();
            if l.starts_with("-----BEGIN") || l.starts_with("-----END") { continue; }
            b64.push_str(l);
        }
        let der = general_purpose::STANDARD.decode(b64)?;
        let sk = SigningKey::from_pkcs8_der(&der)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("ed25519 pkcs8: {}", e)))?;
        Ok(sk)
    } else {
        match general_purpose::STANDARD.decode(trimmed) {
            Ok(raw) => {
                if raw.len() == 32 {
                    let arr: [u8; 32] = raw.as_slice().try_into().map_err(|_| "ed25519 key length")?;
                    Ok(SigningKey::from_bytes(&arr))
                } else {
                    let sk = SigningKey::from_pkcs8_der(&raw)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("ed25519 pkcs8: {}", e)))?;
                    Ok(sk)
                }
            }
            Err(_) => {
                let raw = hex::decode(trimmed)?;
                let arr: [u8; 32] = raw.as_slice().try_into().map_err(|_| "ed25519 key length")?;
                Ok(SigningKey::from_bytes(&arr))
            }
        }
    }
}

async fn ws_api_request(key_id: &str, signing_key: &SigningKey, method: &str, mut params: serde_json::Map<String, serde_json::Value>) -> Result<serde_json::Value, E> {
    let ws_url = "wss://ws-fapi.binance.com/ws-fapi/v1";

    let timestamp = chrono::Utc::now().timestamp_millis();
    params.insert("timestamp".to_string(), serde_json::Value::from(timestamp));
    params.insert("apiKey".to_string(), serde_json::Value::from(key_id.to_string()));

    let mut items: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or(&v.to_string()).to_string()))
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let payload = items
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&");

    let sig = signing_key.sign(payload.as_bytes());
    let sig_b64 = general_purpose::STANDARD.encode(sig.to_bytes());
    params.insert("signature".to_string(), serde_json::Value::from(sig_b64));

    let req_id = format!("debug-{}", timestamp);
    let req = json!({
        "id": req_id,
        "method": method,
        "params": params,
    });

    let (mut ws, _) = connect_async(ws_url).await?;
    ws.send(Message::Text(req.to_string())).await?;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("ws api timeout waiting response".into());
        }
        let remain = deadline - std::time::Instant::now();
        let next = tokio::time::timeout(remain, ws.next()).await?;
        match next {
            Some(Ok(Message::Text(txt))) => {
                println!("[WS-API] recv: {}", txt);
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                    if let Some(status) = v.get("status").and_then(|s| s.as_i64()) {
                        if status == 200 {
                            return Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null));
                        } else {
                            return Err(format!("ws api error: {}", txt).into());
                        }
                    }
                }
            }
            Some(Ok(Message::Binary(_))) => {}
            Some(Ok(Message::Close(reason))) => {
                return Err(format!("ws api closed: {:?}", reason).into());
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(format!("ws api recv: {}", e).into()),
            None => return Err("ws api stream ended".into()),
        }
    }
}

async fn open_listen_key_and_subscribe(rest_api_key: &str) -> Result<(), E> {
    let client = reqwest::Client::new();
    let base_url = "https://fapi.binance.com";

    let resp = client
        .post(format!("{}/fapi/v1/listenKey", base_url))
        .header("X-MBX-APIKEY", rest_api_key)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let txt = resp.text().await.unwrap_or_default();
        return Err(format!("listenKey http {}: {}", status, txt).into());
    }
    let v: serde_json::Value = resp.json().await?;
    let listen_key = v.get("listenKey").and_then(|x| x.as_str()).ok_or("missing listenKey")?.to_string();
    println!("[USER-WS] listenKey acquired");

    let ws_base = "wss://fstream.binance.com";
    let ws_url = format!("{}/ws/{}", ws_base, listen_key);
    let (mut ws, _) = connect_async(&ws_url).await?;
    println!("[USER-WS] connected {}", ws_url);

    let until = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < until {
        if let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(1500), ws.next()).await {
            match msg {
                Ok(Message::Text(txt)) => println!("[USER-WS] {}", txt),
                Ok(Message::Binary(b)) => println!("[USER-WS] (binary) {} bytes", b.len()),
                Ok(Message::Ping(_)) => {}
                Ok(Message::Pong(_)) => {}
                Ok(Message::Close(r)) => { println!("[USER-WS] close: {:?}", r); break; }
                Ok(_) => {}
                Err(e) => { println!("[USER-WS] err: {}", e); break; }
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), E> {
    // Install rustls crypto provider (required for rustls 0.23+)
    let _ = rustls::crypto::ring::default_provider().install_default();

    let default_keys_dir = "/Users/lembrd/lmsol/rust/xtrader/binanance_keys";
    let pem_path = std::env::var("BINANCE_WS_PRIVKEY_PEM").unwrap_or_else(|_| format!("{}/id_ed25519", default_keys_dir));
    let key_id = std::env::var("BINANCE_WS_KEY_ID").unwrap_or_else(|_| "5spbppGxHkEGy9WiWhoUvTtCNffBhh1BBpMmCvx5ksUv4mUJpbbcL4qrgm0VeDZr".to_string());
    let rest_api_key = std::env::var("BINANCE_REST_API_KEY").unwrap_or_else(|_| {
        let yaml_path = "/Users/lembrd/lmsol/rust/xtrader/naive_mm.yaml";
        if let Ok(content) = std::fs::read_to_string(yaml_path) {
            for line in content.lines() {
                let t = line.trim();
                if t.starts_with("api_key:") {
                    let v = t.splitn(2, ':').nth(1).unwrap_or("").trim().trim_matches('"');
                    if !v.is_empty() { return v.to_string(); }
                }
            }
        }
        String::new()
    });

    if rest_api_key.is_empty() {
        println!("[WARN] BINANCE_REST_API_KEY not set and naive_mm.yaml fallback failed; user stream test will be skipped");
    }

    println!("Using key_id={} pem_path={} (not printed)", key_id, pem_path);
    let signing_key = read_pkcs8_pem(&pem_path)?;

    let params = serde_json::Map::new();
    // Optional: request positions for a specific symbol; comment out to get all positions
    // params.insert("symbol".into(), "BTCUSDT".into());
    match ws_api_request(&key_id, &signing_key, "v2/account.position", params).await {
        Ok(result) => println!("[WS-API] v2/account.position OK: {}", result),
        Err(e) => {
            eprintln!("[WS-API] v2/account.position FAILED: {}", e);
        }
    }

    if !rest_api_key.is_empty() {
        if let Err(e) = open_listen_key_and_subscribe(&rest_api_key).await {
            eprintln!("[USER-WS] subscription FAILED: {}", e);
        }
    }

    Ok(())
}


