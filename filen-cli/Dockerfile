# must be built from the filen-rs repository root
# cd .. && docker build -f filen-cli/Dockerfile . -t filen/cli && cd filen-cli

FROM rust:1.91.0-slim AS build
RUN apt-get update \
    && apt-get install -y cmake clang libc++-dev libc++abi-dev \
    && apt-get install -y musl-tools musl-dev
WORKDIR /usr/src/filen-rs
COPY . .
RUN rustup target add x86_64-unknown-linux-musl
RUN cargo build --release --verbose --locked -p filen-cli --target x86_64-unknown-linux-musl -F is_docker

FROM debian:bullseye-slim
COPY --from=build /usr/src/filen-rs/target/x86_64-unknown-linux-musl/release/filen-cli /usr/local/bin/filen-cli
ENTRYPOINT ["filen-cli"]

# todo: make rclone work (it needs glibc)