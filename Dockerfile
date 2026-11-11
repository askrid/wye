# Wye gateway image: a release build on distroless (glibc, no shell, non-root).
#
# From source, in a clone (BINARY=source, the default):
#
#   docker build -t wye .
#   docker run -p 8080:8080 -v ./wye.toml:/etc/wye/wye.toml:ro -e ANTHROPIC_API_KEY wye
#
# The release (.github/workflows/image.yml) builds the published image from
# the binaries it has already published and attested instead, unchanged (so
# not stripped), for both platforms at once with no compiler and no
# emulation: the runtime stage only copies files. The context is a directory
# holding linux-amd64/wye and linux-arm64/wye:
#
#   docker buildx build --platform linux/amd64,linux/arm64 \
#       --build-arg BINARY=prebuilt -f Dockerfile <dir>
#
# BuildKit (Docker's default builder since 23.0) builds only the stages the
# chosen binary needs. Its cache mounts keep Cargo's downloads and target/
# between source builds, so a rebuild after a source change compiles only Wye.
#
# Inside a container Wye must listen on 0.0.0.0, which requires an [auth]
# section (docs/policy.md). scripts/image-smoke.sh builds and checks the image.
#
# Base images are pinned by digest; the tag is for reading. Dependabot bumps
# both together.

# Where /wye comes from: `source` or `prebuilt`, the stages below.
ARG BINARY=source

FROM rust:1.99.1-slim-bookworm@sha256:ed2c95ca377d6571b124b05e75e01207973ded639dce7d5542eb7bcd797e32aa AS source
WORKDIR /src
COPY . .
# target/ is a cache mount, gone after this RUN, so the binary is copied out
# within it. This build strips the binary, so line tables (the release
# profile's debug setting) would be built only to be thrown away.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target,sharing=locked \
    CARGO_PROFILE_RELEASE_DEBUG=false cargo build --release --locked -p wye \
    && cp target/release/wye /wye \
    && strip /wye

FROM scratch AS prebuilt
ARG TARGETARCH
COPY linux-${TARGETARCH}/wye /wye

FROM ${BINARY} AS binary

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
# The release adds version and revision, and copies these to the index of the
# two-platform image, where GHCR reads them.
LABEL org.opencontainers.image.title=wye \
      org.opencontainers.image.description="A small, stateless LLM gateway, configured by one TOML file" \
      org.opencontainers.image.source=https://github.com/askrid/wye \
      org.opencontainers.image.licenses=MIT
COPY --from=binary /wye /usr/local/bin/wye
EXPOSE 8080
# No curl in the image: wye probes itself.
HEALTHCHECK --interval=10s --timeout=6s --start-period=5s \
    CMD ["/usr/local/bin/wye", "probe", "http://127.0.0.1:8080/readyz"]
ENTRYPOINT ["/usr/local/bin/wye"]
ENV WYE_CONFIG=/etc/wye/wye.toml
CMD ["serve"]
