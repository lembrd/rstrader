use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let resp = ureq::get(url).call()?;
    Ok(resp.into_string()?)
}

fn http_post(url: &str, body: &str) -> Result<String, Box<dyn std::error::Error>> {
    let resp = ureq::post(url).send_string(body)?;
    Ok(resp.into_string().unwrap_or_default())
}

#[test]
fn questdb_ilp_and_sql_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("RUN_QUESTDB_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipping questdb integration (set RUN_QUESTDB_TESTS=1)");
        return Ok(());
    }

    // 1) Health check
    let _ = http_get("http://127.0.0.1:9000/exec?query=select%201");

    // 2) Create tables (DDL)
    let ddl_l2 = r#"
CREATE TABLE IF NOT EXISTS l2_updates (
  ts TIMESTAMP,
  rcv_ts TIMESTAMP,
  exchange SYMBOL,
  ticker SYMBOL,
  side SYMBOL,
  action SYMBOL,
  seq_id LONG,
  packet_id LONG,
  price DOUBLE,
  qty DOUBLE,
  update_id LONG,
  first_update_id LONG
) TIMESTAMP(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS(ts, exchange, ticker, side, price, update_id);
"#;

    let ddl_trades = r#"
CREATE TABLE IF NOT EXISTS trades (
  ts TIMESTAMP,
  rcv_ts TIMESTAMP,
  exchange SYMBOL,
  ticker SYMBOL,
  side SYMBOL,
  seq_id LONG,
  packet_id LONG,
  trade_id STRING,
  order_id STRING,
  price DOUBLE,
  qty DOUBLE
) TIMESTAMP(ts) PARTITION BY DAY WAL DEDUP UPSERT KEYS(ts, exchange, ticker, trade_id);
"#;

    // Use HTTP SQL endpoint
    let _ = http_get(&format!(
        "http://127.0.0.1:9000/exec?query={}",
        urlencoding::encode(ddl_l2)
    ))?;
    let _ = http_get(&format!(
        "http://127.0.0.1:9000/exec?query={}",
        urlencoding::encode(ddl_trades)
    ))?;

    // 3) Send ILP batch over TCP (nanoseconds timestamps)
    let mut stream = TcpStream::connect_timeout(&"127.0.0.1:9009".parse().unwrap(), Duration::from_secs(3))?;
    stream.set_nodelay(true)?;

    let ts_ns: i64 = 1_640_995_200_000_000_000; // 2022-01-01T00:00:00Z
    let rcv_ns: i64 = ts_ns + 1_000_000; // +1ms

    let l2_line = format!(
        "l2_updates,exchange=BINANCE_FUTURES,ticker=BTCUSDT,side=BID,action=UPDATE seq_id=1i,packet_id=1i,price=50000.0,qty=1.5,update_id=123i,first_update_id=122i,rcv_ts={}i {}\n",
        rcv_ns, ts_ns
    );
    let trade_line = format!(
        "trades,exchange=BINANCE_FUTURES,ticker=BTCUSDT,side=BUY seq_id=1i,packet_id=1i,trade_id=\"T123456\",order_id=\"O789\",price=50000.0,qty=0.5,rcv_ts={}i {}\n",
        rcv_ns, ts_ns
    );

    stream.write_all(l2_line.as_bytes())?;
    stream.write_all(trade_line.as_bytes())?;
    stream.flush()?;

    std::thread::sleep(Duration::from_millis(700));

    // 4) Verify via HTTP SQL
    let resp_l2 = http_get("http://127.0.0.1:9000/exec?query=select%20count()%20from%20l2_updates")?;
    assert!(resp_l2.contains("dataset"));

    let resp_tr = http_get("http://127.0.0.1:9000/exec?query=select%20count()%20from%20trades")?;
    assert!(resp_tr.contains("dataset"));

    // Spot-check a value
    let q = "select cast(price as long) as p from trades where trade_id='T123456'";
    let resp_check = http_get(&format!(
        "http://127.0.0.1:9000/exec?query={}",
        urlencoding::encode(q)
    ))?;
    assert!(resp_check.contains("50000"), "unexpected response: {}", resp_check);

    Ok(())
}


