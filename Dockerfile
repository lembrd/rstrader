FROM rust:1.78 as builder
WORKDIR /app

# Cache dependencies first (optional speed-up if Docker Build Cloud cache is cold)
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main(){}" > src/main.rs && cargo build --release --locked || true

# Copy full source and build
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd -m -u 10001 runner \
  && apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata \
  && rm -rf /var/lib/apt/lists/*
USER runner
WORKDIR /home/runner
COPY --from=builder /app/target/release/xtrader /usr/local/bin/xtrader
ENTRYPOINT ["/usr/local/bin/xtrader"]


