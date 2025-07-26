# Binance USD-Margined Futures API Research Report

## Executive Summary

This document provides comprehensive research on the Binance USD-margined futures API, covering data schemas, endpoints, WebSocket streams, error handling, rate limits, and implementation requirements for building a high-performance feed handler.

## API Overview

### Base Configuration
- **Production Base URL:** `https://fapi.binance.com`
- **Testnet Base URL:** `https://testnet.binancefuture.com`
- **WebSocket Base URL:** `wss://fstream.binance.com`
- **Protocol:** HTTPS/WSS with TLS 1.2+

### Authentication Methods
- **HMAC SHA256:** Standard signature-based authentication
- **RSA Keys:** Alternative signature method for enhanced security
- **API Key Types:**
  - TRADE: Requires API key + signature
  - USER_DATA: Requires API key + signature  
  - USER_STREAM: Requires API key only
  - MARKET_DATA: Requires API key only
  - NONE: Public endpoints

### Request Parameters
- **Timestamp:** Required for signed endpoints (milliseconds)
- **RecvWindow:** Request validity window (recommended ≤5000ms)
- **Signature:** HMAC SHA256 hash of query string

## Market Data REST API Endpoints

### Core Market Data Endpoints

#### 1. Test Connectivity
- **Endpoint:** `GET /fapi/v1/ping`
- **Authentication:** None
- **Weight:** 1
- **Response:** `{}`

#### 2. Server Time
- **Endpoint:** `GET /fapi/v1/time`
- **Authentication:** None
- **Response:** `{"serverTime": 1638747660000}`

#### 3. Exchange Information
- **Endpoint:** `GET /fapi/v1/exchangeInfo`
- **Authentication:** None
- **Purpose:** Symbol list, trading rules, rate limits
- **Response:** Contains symbols, filters, rateLimits

#### 4. Order Book Depth
- **Endpoint:** `GET /fapi/v1/depth`
- **Authentication:** None
- **Parameters:** symbol (required), limit (5,10,20,50,100,500,1000)
- **Response Schema:**
```json
{
  "lastUpdateId": 1027024,
  "bids": [["4.00000000", "431.00000000"]],
  "asks": [["4.00000200", "12.00000000"]]
}
```

#### 5. Recent Trades
- **Endpoint:** `GET /fapi/v1/trades`
- **Authentication:** None
- **Parameters:** symbol (required), limit (default 500, max 1000)
- **Response Schema:**
```json
[{
  "id": 28457,
  "price": "4.00000100",
  "qty": "12.00000000",
  "time": 1499865549590,
  "isBuyerMaker": true
}]
```

#### 6. Kline/Candlestick Data
- **Endpoint:** `GET /fapi/v1/klines`
- **Authentication:** None
- **Parameters:** symbol, interval, startTime, endTime, limit
- **Intervals:** 1m,3m,5m,15m,30m,1h,2h,4h,6h,8h,12h,1d,3d,1w,1M
- **Response Schema:**
```json
[[
  1499040000000,      // Open time
  "0.01634790",       // Open
  "0.80000000",       // High
  "0.01575800",       // Low
  "0.01577100",       // Close
  "148976.11427815",  // Volume
  1499644799999,      // Close time
  "2434.19055334",    // Quote asset volume
  308,                // Number of trades
  "1756.87402397",    // Taker buy base asset volume
  "28.46694368",      // Taker buy quote asset volume
  "17928899.62484339" // Ignore
]]
```

#### 7. 24hr Ticker Statistics
- **Endpoint:** `GET /fapi/v1/ticker/24hr`
- **Authentication:** None
- **Parameters:** symbol (optional - all symbols if omitted)

#### 8. Symbol Price Ticker
- **Endpoint:** `GET /fapi/v1/ticker/price`
- **Authentication:** None
- **Parameters:** symbol (optional)

#### 9. Symbol Order Book Ticker
- **Endpoint:** `GET /fapi/v1/ticker/bookTicker`
- **Authentication:** None
- **Parameters:** symbol (optional)

#### 10. Open Interest
- **Endpoint:** `GET /fapi/v1/openInterest`
- **Authentication:** None
- **Parameters:** symbol (required)

## WebSocket Market Streams

### Connection Details
- **Base URL:** `wss://fstream.binance.com`
- **Single Stream:** `/ws/<streamName>`
- **Combined Streams:** `/stream?streams=<stream1>/<stream2>`
- **Connection Lifetime:** 24 hours maximum
- **Ping Interval:** Server sends ping every 3 minutes
- **Rate Limit:** 10 incoming messages per second
- **Stream Limit:** Up to 1024 streams per connection

### Available Stream Types

#### 1. Aggregate Trade Streams
- **Stream Name:** `<symbol>@aggTrade`
- **Update Speed:** Real-time
- **Schema:**
```json
{
  "e": "aggTrade",
  "E": 123456789,
  "s": "BTCUSDT",
  "a": 12345,
  "p": "0.001",
  "q": "100",
  "f": 100,
  "l": 105,
  "T": 123456785,
  "m": true
}
```

#### 2. Kline/Candlestick Streams
- **Stream Name:** `<symbol>@kline_<interval>`
- **Update Speed:** 250ms
- **Intervals:** 1m,3m,5m,15m,30m,1h,2h,4h,6h,8h,12h,1d,3d,1w,1M
- **Schema:**
```json
{
  "e": "kline",
  "E": 1638747660000,
  "s": "BTCUSDT",
  "k": {
    "t": 1638747660000,
    "T": 1638747719999,
    "s": "BTCUSDT",
    "i": "1m",
    "f": 100,
    "L": 200,
    "o": "0.0010",
    "c": "0.0020",
    "h": "0.0025",
    "l": "0.0015",
    "v": "1000",
    "n": 100,
    "x": false,
    "q": "1.0000",
    "V": "500",
    "Q": "0.500",
    "B": "123456"
  }
}
```

#### 3. Individual Symbol Mini Ticker
- **Stream Name:** `<symbol>@miniTicker`
- **Update Speed:** 1000ms
- **Schema:**
```json
{
  "e": "24hrMiniTicker",
  "E": 1638747660000,
  "s": "BTCUSDT",
  "c": "0.0025",
  "o": "0.0010",
  "h": "0.0025",
  "l": "0.0010",
  "v": "10000",
  "q": "18"
}
```

#### 4. Individual Symbol Ticker
- **Stream Name:** `<symbol>@ticker`
- **Update Speed:** 1000ms
- **Comprehensive 24hr statistics**

#### 5. Individual Symbol Book Ticker
- **Stream Name:** `<symbol>@bookTicker`
- **Update Speed:** Real-time
- **Schema:**
```json
{
  "e": "bookTicker",
  "u": 400900217,
  "s": "BNBUSDT",
  "b": "25.35190000",
  "B": "31.21000000",
  "a": "25.36520000",
  "A": "40.66000000"
}
```

#### 6. Partial Book Depth Streams
- **Stream Name:** `<symbol>@depth<levels>[@100ms]`
- **Levels:** 5, 10, 20
- **Update Speed:** 250ms, 500ms, or 100ms
- **Schema:**
```json
{
  "lastUpdateId": 160,
  "bids": [["0.0024", "10"]],
  "asks": [["0.0026", "100"]]
}
```

#### 7. Diff Book Depth Streams
- **Stream Name:** `<symbol>@depth[@100ms]`
- **Update Speed:** 250ms, 500ms, or 100ms
- **Purpose:** Order book delta updates
- **Critical for maintaining accurate order book**

#### 8. Mark Price Stream
- **Stream Name:** `<symbol>@markPrice[@1s]`
- **Update Speed:** 3000ms or 1000ms
- **Schema:**
```json
{
  "e": "markPriceUpdate",
  "E": 1562305380000,
  "s": "BTCUSDT",
  "p": "11794.15000000",
  "P": "11806.95000000",
  "r": "0.00038167",
  "T": 1562306400000
}
```

#### 9. All Market Streams
- **All Market Tickers:** `!ticker@arr`
- **All Market Mini Tickers:** `!miniTicker@arr`
- **All Book Tickers:** `!bookTicker`
- **All Mark Prices:** `!markPrice@arr[@1s]`

## Rate Limits & Restrictions

### REST API Rate Limits
- **IP-based limits:** Varies per endpoint
- **Order rate limits:** Account-specific limits
- **Rate limit headers:** X-MBX-USED-WEIGHT, X-MBX-ORDER-COUNT
- **429 Status Code:** Rate limit exceeded
- **Penalties:** Temporary IP bans for repeated violations

### WebSocket Limitations
- **Connection lifetime:** 24 hours maximum
- **Message rate:** 10 incoming messages per second
- **Stream subscriptions:** Up to 1024 streams per connection
- **Concurrent connections:** No documented IP limit
- **Keepalive:** Required ping/pong every 3 minutes

### Recommended Practices
- Monitor rate limit headers
- Implement exponential backoff
- Use WebSocket streams for real-time data
- Batch REST API calls when possible
- Maintain connection pools for redundancy

## Error Handling

### HTTP Status Codes
- **200:** Success
- **400:** Bad Request (malformed request)
- **401:** Unauthorized (invalid API key)
- **403:** Forbidden (IP banned)
- **429:** Too Many Requests (rate limit exceeded)
- **418:** IP autobanned
- **5xx:** Server errors

### Error Response Format
```json
{
  "code": -1121,
  "msg": "Invalid symbol."
}
```

### Common Error Codes
- **-1003:** TOO_MANY_REQUESTS
- **-1021:** INVALID_TIMESTAMP  
- **-1022:** INVALID_SIGNATURE
- **-1100:** ILLEGAL_CHARS
- **-1121:** INVALID_SYMBOL
- **-1102:** MANDATORY_PARAM_EMPTY_OR_MALFORMED

### WebSocket Error Handling
- **Connection drops:** Automatic reconnection with exponential backoff
- **Invalid subscriptions:** Error messages via WebSocket
- **Rate limit violations:** Connection termination
- **Network issues:** Client-side detection and recovery

## Data Schema Mapping for Feed Handler

### Unified Order Book L2 Schema
```rust
struct OrderBookL2 {
    // Common fields
    timestamp: i64,        // Exchange timestamp (microseconds)
    rcv_timestamp: i64,    // Local receive timestamp
    exchange: ExchangeId,  // BINANCE_FUTURES
    ticker: String,        // Symbol (e.g., "BTCUSDT")
    seq_id: i64,          // Local sequence ID
    packet_id: i64,       // Packet grouping ID
    
    // OrderBook specific
    action: Action,        // UPDATE (for depth streams)
    side: Side,           // BID=1, ASK=-1
    price: f64,           // Price level
    qty: f64,             // Quantity at level
}
```

### Unified Trade Schema
```rust
struct Trade {
    // Common fields
    timestamp: i64,        // Time field from aggTrade
    rcv_timestamp: i64,    // Local receive timestamp
    exchange: ExchangeId,  // BINANCE_FUTURES
    ticker: String,        // Symbol
    seq_id: i64,          // Local sequence ID
    packet_id: i64,       // Packet grouping ID
    
    // Trade specific
    side: Side,           // BUY=1 if !m, SELL=-1 if m
    price: f64,           // p field
    qty: f64,             // q field
}
```

### Unified Kline Schema
```rust
struct Kline {
    // Common fields  
    timestamp: i64,        // k.t (kline start time)
    rcv_timestamp: i64,    // Local receive timestamp
    exchange: ExchangeId,  // BINANCE_FUTURES
    ticker: String,        // k.s symbol
    seq_id: i64,          // Local sequence ID
    packet_id: i64,       // Packet grouping ID
    
    // Kline specific
    interval: String,      // k.i interval
    open: f64,            // k.o open price
    high: f64,            // k.h high price  
    low: f64,             // k.l low price
    close: f64,           // k.c close price
    volume: f64,          // k.v volume
    is_closed: bool,      // k.x kline closed flag
}
```

## Implementation Recommendations

### Connection Management
1. **Multiple Connections:** Use connection pools for redundancy
2. **Health Monitoring:** Track connection status and latency
3. **Automatic Reconnection:** Exponential backoff with jitter
4. **Graceful Shutdown:** Proper WebSocket close handshake

### Data Integrity
1. **Sequence Numbers:** Use lastUpdateId for order book synchronization
2. **Gap Detection:** Monitor for missing sequence numbers
3. **Recovery Strategy:** REST snapshot + WebSocket delta reconstruction
4. **Validation:** Schema validation for all incoming messages

### Performance Optimization
1. **Async Processing:** Use tokio for non-blocking I/O
2. **Message Batching:** Group messages by packet arrival
3. **Memory Management:** Efficient parsing and minimal allocations
4. **Compression:** Consider WebSocket compression for bandwidth

### Monitoring & Alerting
1. **Latency Tracking:** Measure end-to-end message latency
2. **Connection Health:** Monitor reconnection frequency
3. **Data Quality:** Track missing/invalid messages
4. **Rate Limit Monitoring:** Watch for approaching limits

### Error Recovery
1. **Circuit Breaker:** Prevent cascade failures
2. **Dead Letter Queue:** Handle unprocessable messages
3. **Alerting:** Telegram notifications for critical failures
4. **Manual Intervention:** Clear escalation procedures

## Security Considerations

### API Key Management
- Store keys securely (environment variables, vault)
- Use read-only keys for market data
- Rotate keys regularly
- Monitor for unauthorized usage

### Network Security
- Use TLS 1.2+ for all connections
- Validate server certificates
- Consider IP whitelisting
- Monitor for suspicious activity

## Testing Strategy

### Unit Testing
- Message parsing validation
- Schema conversion accuracy
- Error handling scenarios
- Rate limit simulation

### Integration Testing
- Live connection testing
- Multi-stream subscriptions
- Reconnection scenarios
- Performance benchmarking

### Load Testing
- High-frequency message handling
- Memory usage under load
- Connection pool management
- Error recovery testing

## Conclusion

The Binance USD-margined futures API provides comprehensive market data access through both REST and WebSocket interfaces. The implementation should focus on:

1. **Robust connection management** with automatic recovery
2. **Accurate data synchronization** using sequence numbers
3. **Low-latency processing** with async architecture
4. **Comprehensive monitoring** and alerting
5. **Graceful error handling** and recovery

The unified data schema design allows for seamless integration with the multi-exchange feed handler architecture while maintaining data consistency and enabling efficient storage in Parquet format.