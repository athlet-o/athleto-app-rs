# syntax=docker/dockerfile:1.7

FROM rust:1.90-bookworm AS build
ARG TARGETARCH
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=locked \
    --mount=type=cache,target=/app/target,id=athleto-app-rs-target-${TARGETARCH},sharing=locked \
    cargo build --release \
 && cp target/release/athleto-app-rs /usr/local/bin/athleto-app-rs

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && apt-get clean

COPY --from=build /usr/local/bin/athleto-app-rs /usr/local/bin/athleto-app-rs

ENV HOST=0.0.0.0 \
    PORT=8080

EXPOSE 8080
USER 10001:10001
CMD ["/usr/local/bin/athleto-app-rs"]
