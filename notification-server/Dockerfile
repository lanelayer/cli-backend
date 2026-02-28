# Use the official Rust image as a builder (matching Debian Bookworm for GLIBC compatibility)
FROM rust:1-bookworm AS builder
WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm src/main.rs
COPY src ./src
RUN touch src/main.rs && cargo build --release
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y \
    curl \
    ca-certificates \
    docker.io \
    strace \
    file \
    libc6 \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y nodejs \
    && npm install -g @lanelayer/cli

RUN mkdir -p /usr/local/lib/docker/cli-plugins/ && \
    curl -SL https://github.com/docker/buildx/releases/download/v0.12.1/buildx-v0.12.1.linux-amd64 -o /usr/local/lib/docker/cli-plugins/docker-buildx && \
    chmod +x /usr/local/lib/docker/cli-plugins/docker-buildx && \
    mkdir -p /usr/lib/docker/cli-plugins/ && \
    ln -sf /usr/local/lib/docker/cli-plugins/docker-buildx /usr/lib/docker/cli-plugins/docker-buildx

COPY --from=builder /usr/src/app/target/release/notification-server /usr/local/bin/notification-server
RUN chmod +x /usr/local/bin/notification-server
COPY start.sh /usr/local/bin/start.sh
RUN chmod +x /usr/local/bin/start.sh
EXPOSE 8000
CMD ["/usr/local/bin/start.sh"]