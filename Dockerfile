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
  && apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata wget \
  && rm -rf /var/lib/apt/lists/*

# Install Flyway Commandline (bundled JRE)
ARG FLYWAY_VERSION=9.22.3
RUN set -euo pipefail \
  && cd /tmp \
  && wget -q https://repo1.maven.org/maven2/org/flywaydb/flyway-commandline/${FLYWAY_VERSION}/flyway-commandline-${FLYWAY_VERSION}-linux-x64.tar.gz \
  && tar -xzf flyway-commandline-${FLYWAY_VERSION}-linux-x64.tar.gz \
  && mv flyway-${FLYWAY_VERSION} /opt/flyway \
  && ln -s /opt/flyway/flyway /usr/local/bin/flyway \
  && rm -f flyway-commandline-${FLYWAY_VERSION}-linux-x64.tar.gz

WORKDIR /home/runner
COPY --from=builder /app/target/release/xtrader /usr/local/bin/xtrader
COPY --from=builder /app/sql/pg /usr/local/share/xtrader/migrations/pg

# Wrapper entrypoint to run migrations then start app
COPY scripts/docker_entrypoint.sh /usr/local/bin/docker_entrypoint.sh
RUN chmod +x /usr/local/bin/docker_entrypoint.sh \
  && chown -R 10001:10001 /usr/local/share/xtrader
USER runner
ENTRYPOINT ["/usr/local/bin/docker_entrypoint.sh"]


