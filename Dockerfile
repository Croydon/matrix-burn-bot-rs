FROM mcr.microsoft.com/devcontainers/rust:2-1-trixie AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:trixie-slim

COPY --from=builder /app/target/release/matrix-burn-bot /app/matrix-burn-bot
WORKDIR /app
CMD ["./matrix-burn-bot"]
