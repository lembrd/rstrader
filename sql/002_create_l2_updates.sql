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


