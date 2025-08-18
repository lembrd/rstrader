FROM rust:1.82 AS builder
WORKDIR /app

# Cache dependencies first (optional speed-up if Docker Build Cloud cache is cold)
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo "fn main(){}" > src/main.rs && cargo build --release --locked || true

# Copy full source and build
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd -m -u 10001 runner \
  && apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata wget tar gzip openjdk-17-jre-headless \
  && rm -rf /var/lib/apt/lists/*

FROM flyway/flyway:11.11.1 AS flyway

FROM debian:bookworm-slim
RUN useradd -m -u 10001 runner \
  && apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata wget tar gzip openjdk-17-jre-headless \
  && rm -rf /var/lib/apt/lists/*

# Install Flyway (copy from official image; includes JRE)
COPY --from=flyway /flyway /opt/flyway
RUN ln -s /opt/flyway/flyway /usr/local/bin/flyway \
  && chmod -R a+rx /opt/flyway \
  && chmod a+rx /usr/local/bin/flyway

WORKDIR /home/runner
COPY --from=builder /app/target/release/xtrader /usr/local/bin/xtrader
RUN mkdir -p /usr/local/share/xtrader/migrations/pg
COPY --from=builder /app/sql/pg/V*.sql /usr/local/share/xtrader/migrations/pg/

# Wrapper entrypoint to run migrations then start app
COPY scripts/docker_entrypoint.sh /usr/local/bin/docker_entrypoint.sh
RUN chmod +x /usr/local/bin/docker_entrypoint.sh \
  && chown -R 10001:10001 /usr/local/share/xtrader /opt/flyway
USER runner
ENTRYPOINT ["/usr/local/bin/docker_entrypoint.sh"]


