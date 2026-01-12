# must be built from the filen-rs repository root
# cd .. && docker build -f filen-cli/Dockerfile . -t filen/cli && cd filen-cli

FROM rust:1.91.0-slim AS build
RUN apt-get update \
    && apt-get install -y cmake clang libc++-dev libc++abi-dev \
    && apt-get install -y musl-tools musl-dev
WORKDIR /usr/src/filen-rs
COPY . .
ARG TARGETARCH
RUN if [ "$TARGETARCH" = "amd64" ]; then \
    arch="x86_64"; \
    elif [ "$TARGETARCH" = "arm64" ]; then \
    arch="aarch64"; \
    else \
    echo "Unsupported architecture"; exit 1; \
    fi && \
    rustup target add ${arch}-unknown-linux-musl && \
    cargo build --release --verbose --locked -p filen-cli --target ${arch}-unknown-linux-musl -F is_docker && \
    cp /usr/src/filen-rs/target/${arch}-unknown-linux-musl/release/filen-cli /filen-cli

FROM alpine:latest
# runtime dependencies needed for rclone
RUN apk --no-cache add ca-certificates fuse libc6-compat
COPY --from=build /filen-cli /usr/local/bin/filen-cli
ENTRYPOINT ["filen-cli"]

# could add Docker build with filen-rclone preinstalled, by doing something like:
# ADD https://github.com/FilenCloudDienste/filen-rclone/releases/download/v1.70.0-filen.14/rclone-v1.70.0-filen.14-linux-amd64
# but this fails when users specify a custom config dir (e.g. when mounting it as a volume), so skipping for now