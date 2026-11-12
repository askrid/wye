# Wye gateway image: a release build on distroless (glibc, no shell, non-root).
#
# From source, in a clone (BINARY=source, the default):
#
#   docker build -t wye .
#   docker run -p 8080:8080 -v ./wye.toml:/etc/wye/wye.toml:ro -e ANTHROPIC_API_KEY wye
#
# This builds with the release archives' `dist` profile (debug info stripped,
# symbols kept). The release (.github/workflows/image.yml) builds the
# published image from the binaries it has already published and attested
# instead, unchanged, for both platforms at once with no compiler and no
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
# within it.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --profile dist --locked -p wye \
    && cp target/dist/wye /wye

FROM scratch AS prebuilt
ARG TARGETARCH
COPY linux-${TARGETARCH}/wye /wye

FROM ${BINARY} AS binary

# The runtime base is distroless `cc` without five of its packages: its
# layers are base-nossl's plus libssl3, libgomp1, libstdc++6, libgcc-s1, and
# gcc-12-base. Wye needs only libc, libm, and libgcc_s (for unwinding), so
# libgcc_s.so.1 comes from `cc` itself, with the dpkg records that let SBOM
# and vulnerability scanners see it and its copyright.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS cc

FROM scratch AS libgcc-amd64
COPY --from=cc /lib/x86_64-linux-gnu/libgcc_s.so.1 /lib/x86_64-linux-gnu/
FROM scratch AS libgcc-arm64
COPY --from=cc /lib/aarch64-linux-gnu/libgcc_s.so.1 /lib/aarch64-linux-gnu/

FROM libgcc-${TARGETARCH} AS libgcc
COPY --from=cc /var/lib/dpkg/status.d/libgcc-s1 \
    /var/lib/dpkg/status.d/libgcc-s1.md5sums \
    /var/lib/dpkg/status.d/gcc-12-base \
    /var/lib/dpkg/status.d/gcc-12-base.md5sums /var/lib/dpkg/status.d/
COPY --from=cc /usr/share/doc/gcc-12-base/ /usr/share/doc/gcc-12-base/

FROM gcr.io/distroless/base-nossl-debian12:nonroot@sha256:be40c00dfabd86576d92666e87e406714d5618342de1a0c213ad232de255172e
# The release adds version and revision, and copies these to the index of the
# two-platform image, where GHCR reads them.
LABEL org.opencontainers.image.title=wye \
      org.opencontainers.image.description="A small, stateless LLM gateway, configured by one TOML file" \
      org.opencontainers.image.source=https://github.com/askrid/wye \
      org.opencontainers.image.licenses=MIT
COPY --from=libgcc / /
COPY --from=binary /wye /usr/local/bin/wye
EXPOSE 8080
# No curl in the image: wye probes itself.
HEALTHCHECK --interval=10s --timeout=6s --start-period=5s \
    CMD ["/usr/local/bin/wye", "probe", "http://127.0.0.1:8080/readyz"]
ENTRYPOINT ["/usr/local/bin/wye"]
ENV WYE_CONFIG=/etc/wye/wye.toml
CMD ["serve"]
