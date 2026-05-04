FROM rust:1.85-bookworm AS build
WORKDIR /repo

COPY Cargo.toml Cargo.lock ./
COPY src src
COPY version.json version.json

RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates \
    cloud-image-utils \
    openssh-client \
    pciutils \
    qemu-system-arm \
    qemu-system-x86 \
    qemu-utils \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=build /repo/target/release/statix-agent /app/statix-agent
COPY --from=build /repo/version.json /app/version.json

CMD ["/app/statix-agent"]
