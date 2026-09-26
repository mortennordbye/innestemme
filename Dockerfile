# syntax=docker/dockerfile:1
# check=skip=FromPlatformFlagConstDisallowed
# The runtime stage is pinned to linux/amd64 on purpose: the binaries are x86-64-v3 only.

# Runs on the build machine's own architecture and cross-compiles to x86-64, so a build on an
# arm64 host does not go through emulation.
FROM --platform=$BUILDPLATFORM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS chef
ARG BUILDARCH
WORKDIR /src
# The toolchain and target-cpu settings come first: dependencies compiled without them would be
# rebuilt in the last step, and the cached layer would be worthless.
COPY rust-toolchain.toml ./
COPY .cargo .cargo
RUN if [ "$BUILDARCH" != "amd64" ]; then \
      apt-get update \
      && apt-get install -y --no-install-recommends gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
      && rm -rf /var/lib/apt/lists/*; \
    fi \
    && rustup target add x86_64-unknown-linux-gnu \
    && cargo install cargo-chef --version 0.1.78 --locked
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc

# The dependency list only, so the next stage's layer changes when Cargo.lock does and not on
# every source edit.
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Dependencies (candle and the rest, most of the compile time) as their own layer, which the
# registry build cache keeps between CI runs. BuildKit cache mounts are not exported there.
FROM chef AS build
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --target x86_64-unknown-linux-gnu -p voice-engine -p voice-bench \
      --recipe-path recipe.json
COPY . .
# The explicit --target is what makes .cargo/config.toml apply target-cpu=x86-64-v3.
RUN cargo build --release --locked --target x86_64-unknown-linux-gnu -p voice-engine -p voice-bench \
    && mkdir -p /out/bin /out/models \
    && cp target/x86_64-unknown-linux-gnu/release/voice-engine target/x86_64-unknown-linux-gnu/release/voice-bench /out/bin/

FROM --platform=linux/amd64 gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /out/bin/ /usr/local/bin/
# Hugging Face cache. Mount a volume here so the weights survive restarts.
COPY --from=build --chown=nonroot:nonroot /out/models /models
ENV HF_HOME=/models
VOLUME /models
EXPOSE 7000/udp 9090/tcp
USER nonroot
ENTRYPOINT ["/usr/local/bin/voice-engine"]
