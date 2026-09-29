# syntax=docker/dockerfile:1.7
#
# Multi-stage build:
#   builder -> compiles the release binary (whisper.cpp is built from source)
#   test    -> runs the full workspace test suite (docker build --target test .)
#   runtime -> slim Debian image with the binary, web assets, and example config
#
# The whisper.cpp CPU backend is tuned per target architecture:
#   amd64: native CPU detection (build on the machine that will run it)
#   arm64: explicit -march (Debian's GCC 12 cannot detect Apple/Ampere cores);
#          override GGML_CPU_ARM_ARCH=armv8-a for a Raspberry Pi 4.

FROM rust:1.88-bookworm AS builder

ARG TARGETARCH
ARG GGML_CPU_ARM_ARCH=armv8.2-a+fp16+dotprod

RUN apt-get update && \
    apt-get install --yes --no-install-recommends cmake && \
    rm -rf /var/lib/apt/lists/*

RUN if [ "$TARGETARCH" = "arm64" ]; then \
      printf 'export GGML_NATIVE=OFF\nexport GGML_CPU_ARM_ARCH=%s\n' "$GGML_CPU_ARM_ARCH" > /etc/ggml-env.sh; \
    else \
      : > /etc/ggml-env.sh; \
    fi

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY third-party ./third-party
COPY fixtures ./fixtures

ENV WHISPER_DONT_GENERATE_BINDINGS=1

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    . /etc/ggml-env.sh && \
    cargo build --locked --release -p trunkline-server && \
    cp /src/target/release/trunkline /tmp/trunkline

FROM builder AS test

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    . /etc/ggml-env.sh && \
    cargo test --locked --workspace

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install --yes --no-install-recommends ca-certificates libgomp1 && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /tmp/trunkline /usr/local/bin/trunkline
COPY web ./web
COPY config/trunkline.example.toml ./config/trunkline.toml

ENV TRUNKLINE_CONFIG=/app/config/trunkline.toml
ENV RUST_LOG=trunkline_server=info,radio_core=info

EXPOSE 8097
ENTRYPOINT ["/usr/local/bin/trunkline"]
CMD ["serve"]
