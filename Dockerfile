# syntax=docker/dockerfile:1
# check=skip=FromPlatformFlagConstDisallowed
# The runtime stage is pinned to linux/amd64 on purpose: the binaries are x86-64-v3 only.

# Runs on the build machine's own architecture and cross-compiles to x86-64, so a build on an
# arm64 host does not go through emulation.
FROM --platform=$BUILDPLATFORM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build
ARG BUILDARCH
RUN if [ "$BUILDARCH" != "amd64" ]; then \
      apt-get update \
      && apt-get install -y --no-install-recommends gcc-x86-64-linux-gnu libc6-dev-amd64-cross \
      && rm -rf /var/lib/apt/lists/*; \
    fi \
    && rustup target add x86_64-unknown-linux-gnu
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc
WORKDIR /src
COPY . .
# The explicit --target is what makes .cargo/config.toml apply target-cpu=x86-64-v3.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --target x86_64-unknown-linux-gnu -p voice-engine -p voice-bench \
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
