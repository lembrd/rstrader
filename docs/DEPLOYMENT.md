## XTrader Production Deployment Guide

This guide outlines practical options to deploy `xtrader` to production on AWS and private hosting. It focuses on:

- Fast, low-friction rollouts of new versions
- Easy per-instance configuration
- Operational safety and maintainability

The application is a long-running data collector that runs continuously by default (use `--shutdown-after` only for testing). It reads configuration from CLI flags and environment variables and automatically loads a local `.env` file when present.

### Core runtime interface

- Command-line (required):
  - `--subscriptions <SUBS>` (comma-separated `STREAM:EXCHANGE@INSTRUMENT`)
  - `--sink <parquet|questdb>` (default `parquet`)
  - `--output-directory <DIR>` (parquet only)
  - `--verbose` (metrics/telemetry logs)
  - Optional: `--shutdown-after <SECONDS>`
- Environment variables (optional):
  - `DERIBIT_CLIENT_ID`, `DERIBIT_CLIENT_SECRET`, `DERIBIT_USE_TESTNET` (default true)
  - `QUESTDB_HOST` (default `127.0.0.1`), `QUESTDB_ILP_PORT` (default `9009`)
  - `RUST_LOG` (e.g., `info`, `debug`)
- `.env` file is auto-loaded if present.

---

## Recommended packaging: container image

Containerizing `xtrader` enables consistent, rapid rollouts across AWS and private platforms.

Example Dockerfile (multi-stage, optimized for release):

```Dockerfile
FROM rust:1.78 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN useradd -m -u 10001 runner \
  && apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata \
  && rm -rf /var/lib/apt/lists/*
USER runner
WORKDIR /home/runner
COPY --from=builder /app/target/release/xtrader /usr/local/bin/xtrader
ENTRYPOINT ["/usr/local/bin/xtrader"]
```

Build and push:

```bash
docker build -t your-registry/xtrader:1.0.0 .
docker push your-registry/xtrader:1.0.0
```

Tag new releases immutably (e.g., `1.0.1`, `1.0.2`) and use a moving tag (e.g., `stable`) for rollouts if desired.

---

## Configuration patterns (per-instance)

- Environment file per instance
  - Docker Compose: `env_file: .env.instance-a`
  - Kubernetes: `ConfigMap`/`Secret` → env vars via `envFrom`
  - systemd: `EnvironmentFile=/etc/xtrader/instance-a.env`
- CLI flags in a wrapper command
  - Keep instance-specific `subscriptions` in the env and reference it in `ExecStart`/`command`.
- Secrets management
  - AWS: Systems Manager Parameter Store or Secrets Manager
  - Private: Vault or encrypted `.env` distribution (e.g., SOPS)

Example `.env` (per instance):

```bash
RUST_LOG=info
DERIBIT_CLIENT_ID=***
DERIBIT_CLIENT_SECRET=***
DERIBIT_USE_TESTNET=false
QUESTDB_HOST=10.0.1.25
QUESTDB_ILP_PORT=9009
SUBSCRIPTIONS="L2:OKX_SWAP@BTCUSDT,TRADES:BINANCE_FUTURES@ETHUSDT"
```

---

## AWS deployment options

### 1) Amazon ECS on Fargate (managed, no servers)

Fast to operate with rolling updates and per-task env overrides.

- How
  - Push container to ECR
  - Create ECS Task Definition with container `image: <ECR_URI>:<tag>`
  - Configure env vars or SSM Parameter Store/Secrets Manager
  - Service → desired count = number of instances
  - Command example: `--sink questdb --verbose --subscriptions "$SUBSCRIPTIONS"`
- Rolling updates
  - Update service to new image tag; ECS drains old tasks and starts new ones
- Pros
  - Managed compute (no AMIs, no patching)
  - Built-in blue/green and rolling deployment strategies
  - Easy per-instance config via task-level env/SSM
- Cons
  - Higher cost than EC2 for steady workloads
  - Limited low-level tuning vs. EC2

Minimal task container snippet:

```json
{
  "name": "xtrader",
  "image": "<account>.dkr.ecr.<region>.amazonaws.com/xtrader:1.0.0",
  "command": ["--sink", "questdb", "--verbose", "--subscriptions", "${SUBSCRIPTIONS}"] ,
  "environment": [
    {"name": "RUST_LOG", "value": "info"},
    {"name": "QUESTDB_HOST", "value": "questdb.internal"},
    {"name": "QUESTDB_ILP_PORT", "value": "9009"}
  ],
  "secrets": [
    {"name": "DERIBIT_CLIENT_ID", "valueFrom": "arn:aws:ssm:...:parameter/DERIBIT_CLIENT_ID"},
    {"name": "DERIBIT_CLIENT_SECRET", "valueFrom": "arn:aws:secretsmanager:...:secret:deribit"}
  ],
  "logConfiguration": {"logDriver": "awslogs", "options": {"awslogs-group": "/ecs/xtrader", "awslogs-region": "<region>", "awslogs-stream-prefix": "xtrader"}}
}
```

### 2) Amazon EKS (Kubernetes)

Best for orgs already on K8s; powerful rollout and config primitives.

- How
  - Push image to ECR
  - Define `Deployment` with `replicas: N`
  - Use `ConfigMap`/`Secret` for env and per-instance deployments via labels/selectors
  - Optional: node affinity for instance placement
- Rolling updates
  - `kubectl set image` triggers rolling update; `kubectl rollout status` to verify
- Pros
  - Strong control over scheduling, auto-scaling, and lifecycle
  - Native config/secrets and zero-downtime updates
- Cons
  - Operational overhead of Kubernetes control plane and add-ons

Example `Deployment`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: xtrader
spec:
  replicas: 2
  selector: { matchLabels: { app: xtrader } }
  template:
    metadata: { labels: { app: xtrader } }
    spec:
      containers:
        - name: xtrader
          image: <account>.dkr.ecr.<region>.amazonaws.com/xtrader:1.0.0
          args: ["--sink", "questdb", "--verbose", "--subscriptions", "$(SUBSCRIPTIONS)"]
          env:
            - name: RUST_LOG
              value: info
            - name: SUBSCRIPTIONS
              valueFrom:
                configMapKeyRef: { name: xtrader-config, key: subscriptions }
            - name: QUESTDB_HOST
              value: questdb.default.svc.cluster.local
          resources: { requests: { cpu: "200m", memory: "256Mi" }, limits: { cpu: "1", memory: "1Gi" } }
          # Optional liveness/readiness if you add a future HTTP/metrics endpoint
```

### 3) EC2 + systemd (simple and cost-efficient)

Good when you prefer VM control or need custom networking.

- How
  - Install Docker (or run the binary directly)
  - Manage service with `systemd` and an `EnvironmentFile`
- Rolling updates
  - Pull new image and restart service; can stagger across instances via SSM
- Pros
  - Lowest AWS cost at steady utilization
  - Full control over OS/network tuning
- Cons
  - You manage OS patching, scaling, and rollouts

Example `systemd` unit (containerized):

```ini
[Unit]
Description=XTrader market data collector
After=network-online.target docker.service
Wants=network-online.target

[Service]
EnvironmentFile=/etc/xtrader/instance-a.env
Restart=always
RestartSec=5
ExecStartPre=/usr/bin/docker pull your-registry/xtrader:1.0.0
ExecStart=/usr/bin/docker run --rm \
  --name xtrader \
  --env-file /etc/xtrader/instance-a.env \
  your-registry/xtrader:1.0.0 \
  --sink questdb --verbose --subscriptions "$SUBSCRIPTIONS"
ExecStop=/usr/bin/docker stop xtrader

[Install]
WantedBy=multi-user.target
```

---

## Private hosting options

### 1) Docker Compose (single host)

Simple, reliable, and fast for small fleets.

`docker-compose.yml`:

```yaml
services:
  xtrader:
    image: your-registry/xtrader:1.0.0
    env_file: .env.instance-a
    command: ["--sink", "questdb", "--verbose", "--subscriptions", "${SUBSCRIPTIONS}"]
    restart: always
```

Upgrade:

```bash
docker compose pull xtrader && docker compose up -d xtrader
```

Pros

- **Pros**: Very easy, minimal infra, quick rollouts
- **Cons**: Single-host; manual HA and scheduling

### 2) Kubernetes (self-managed)

Same model as EKS; use your in-house registry and cluster.

- **Pros**: Scales well; strong rollout and config tooling
- **Cons**: Operational complexity of running Kubernetes yourself

### 3) Nomad or systemd-supervised binaries

- **Pros**: Lightweight scheduler (Nomad) or very low-overhead (systemd)
- **Cons**: Fewer ecosystem integrations than Kubernetes; DIY for HA/rollouts

---

## Version rollout strategies (fast and easy)

- Immutable image tags per build (e.g., `1.0.3`), optional `stable` tag
- GitHub Actions or similar CI to build/push on merge to `main`
- ECS: Update service to new tag (CLI or UI)
- Kubernetes: `kubectl set image deployment/xtrader xtrader=<image>:1.0.3 && kubectl rollout status deployment/xtrader`
- Compose: `docker compose pull && docker compose up -d`
- EC2 + systemd: bump tag in `EnvironmentFile` or unit and `systemctl restart xtrader`
- Staggered rollouts: update a subset of instances first, then the rest

---

## Per-instance configuration made easy

- Use one image for all instances; vary only environment and subscriptions
- Store per-instance `.env` files alongside infra code (or in SSM/Secrets)
- Reference `SUBSCRIPTIONS` env in the command/args
- For fleets, template configs with your IaC (Terraform/CloudFormation/Helm)

---

## Logging, monitoring, and lifecycle

- Logging: `RUST_LOG=info` by default; ship logs to CloudWatch (ECS), Fluent Bit (K8s), or a central log collector
- Supervision: containers/systemd restart policy ensures long-running operation
- Shutdown: send SIGINT/CTRL+C or set `--shutdown-after` for controlled test runs

---

## Choosing an option: pros and cons summary

- AWS ECS Fargate
  - **Pros**: No servers; easy rollouts; good defaults; integrates with SSM/Secrets
  - **Cons**: Higher per-hour cost; less low-level tuning

- AWS EKS
  - **Pros**: Advanced control; native K8s workflows; excellent rollout strategies
  - **Cons**: Operational overhead; requires K8s expertise

- AWS EC2 + systemd
  - **Pros**: Cost-efficient; simple; full OS control
  - **Cons**: You own patching and rollout orchestration

- Private Docker Compose
  - **Pros**: Easiest path; fast updates; minimal moving parts
  - **Cons**: Single-host; manual HA

- Private Kubernetes/Nomad
  - **Pros**: Scales with strong scheduling and rollout primitives
  - **Cons**: You run the platform; higher complexity

---

## Quick checklist

- Build and push container image per release
- Choose platform (ECS/EKS/EC2 or Compose/K8s/Nomad)
- Define per-instance `.env` or platform-native config/secrets
- Run with `--subscriptions` bound from `SUBSCRIPTIONS` env
- Use rolling update commands native to the platform



