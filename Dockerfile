# Wye gateway image: a release build on distroless (glibc, no shell, non-root).
#
#   docker build -t wye .
#   docker run -p 8080:8080 -v ./wye.toml:/etc/wye/wye.toml:ro -e ANTHROPIC_API_KEY wye
#
# Inside a container Wye must listen on 0.0.0.0, which requires an [auth]
# section (docs/policy.md). scripts/image-smoke.sh builds and checks the image.
#
# Base images are pinned by digest; the tag is for reading. Dependabot bumps
# both together.

FROM rust:1.99.1-slim-bookworm@sha256:ed2c95ca377d6571b124b05e75e01207973ded639dce7d5542eb7bcd797e32aa AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p wye \
    && strip target/release/wye

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
COPY --from=build /src/target/release/wye /usr/local/bin/wye
EXPOSE 8080
# No curl in the image: wye probes itself.
HEALTHCHECK --interval=10s --timeout=6s --start-period=5s \
    CMD ["/usr/local/bin/wye", "probe", "http://127.0.0.1:8080/readyz"]
ENTRYPOINT ["/usr/local/bin/wye"]
ENV WYE_CONFIG=/etc/wye/wye.toml
CMD ["serve"]
