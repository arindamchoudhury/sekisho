# syntax=docker/dockerfile:1
# The sekisho binary and the end-to-end mock model (sekisho-mock-model).
#   docker build -t sekisho .
# The end-to-end stack builds with SEKISHO_FEATURES=sekisho-server/e2e, which
# allows its hook-free comparison route; release images leave it empty.

FROM rust:1.99.1-slim-trixie AS build
# The image's toolchain is the pinned one; skip rust-toolchain.toml's
# development components (clippy, rustfmt).
ENV RUSTUP_TOOLCHAIN=1.99.1
ARG SEKISHO_FEATURES=""
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p sekisho-server -p sekisho-testkit --bins \
        ${SEKISHO_FEATURES:+--features "$SEKISHO_FEATURES"} \
 && mkdir /out \
 && cp target/release/sekisho target/release/sekisho-mock-model /out/

FROM debian:trixie-slim
# agentgateway runs as 65532 and must read the config file the gateway agent
# writes (mode 0600) on a shared volume, so Sekisho runs as 65532 too.
RUN mkdir /config && chown 65532:65532 /config
COPY --from=build /out/ /usr/local/bin/
USER 65532:65532
ENTRYPOINT ["sekisho"]
