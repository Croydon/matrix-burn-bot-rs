FROM mcr.microsoft.com/devcontainers/rust:2-1-trixie AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:trixie-slim

RUN apt-get update \
	&& apt-get install --no-install-recommends --yes ca-certificates \
	&& rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/matrix-burn-bot /app/matrix-burn-bot
WORKDIR /app
ENTRYPOINT ["./matrix-burn-bot"]
