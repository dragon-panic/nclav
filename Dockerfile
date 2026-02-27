# ── Stage 1: Build nclav ─────────────────────────────────────────────────────
FROM rust:1.80-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --locked -p nclav-cli

# ── Stage 2: Download IaC tools ──────────────────────────────────────────────
FROM debian:bookworm-slim AS tools

ARG TERRAFORM_VERSION=1.9.8
ARG TOFU_VERSION=1.8.3

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl unzip ca-certificates \
 && rm -rf /var/lib/apt/lists/*

RUN set -eux; \
    ARCH=$(dpkg --print-architecture); \
    curl -fsSL "https://releases.hashicorp.com/terraform/${TERRAFORM_VERSION}/terraform_${TERRAFORM_VERSION}_linux_${ARCH}.zip" \
         -o /tmp/tf.zip; \
    unzip /tmp/tf.zip -d /usr/local/bin; \
    rm /tmp/tf.zip; \
    terraform version

RUN set -eux; \
    ARCH=$(dpkg --print-architecture); \
    curl -fsSL "https://github.com/opentofu/opentofu/releases/download/v${TOFU_VERSION}/tofu_${TOFU_VERSION}_linux_${ARCH}.zip" \
         -o /tmp/tofu.zip; \
    unzip /tmp/tofu.zip -d /tmp/tofu_extract; \
    mv /tmp/tofu_extract/tofu /usr/local/bin/tofu; \
    rm -rf /tmp/tofu.zip /tmp/tofu_extract; \
    tofu version

# ── Stage 3: Runtime image ───────────────────────────────────────────────────
FROM debian:bookworm-slim

# ca-certificates: TLS for GCP API calls and terraform providers
# git: terraform module sources via git::https://...
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates git \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/nclav /usr/local/bin/nclav
COPY --from=tools   /usr/local/bin/terraform    /usr/local/bin/terraform
COPY --from=tools   /usr/local/bin/tofu         /usr/local/bin/tofu

WORKDIR /app
EXPOSE 8080

ENTRYPOINT ["nclav"]
