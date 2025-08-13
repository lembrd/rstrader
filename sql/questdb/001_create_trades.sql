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



