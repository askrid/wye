# Wye gateway image: a release build on distroless (glibc, no shell, non-root).
#
#   docker build -t wye .
#   docker run -p 8080:8080 -v ./wye.toml:/etc/wye/wye.toml:ro -e ANTHROPIC_API_KEY wye
#
# Inside a container Wye must listen on 0.0.0.0, which requires an [auth]
# section (docs/policy.md). scripts/image-smoke.sh builds and checks the image.

FROM rust:1.98.1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p wye \
    && strip target/release/wye

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/wye /usr/local/bin/wye
EXPOSE 8080
# No curl in the image: wye probes itself.
HEALTHCHECK --interval=10s --timeout=6s --start-period=5s \
    CMD ["/usr/local/bin/wye", "probe", "http://127.0.0.1:8080/readyz"]
ENTRYPOINT ["/usr/local/bin/wye"]
CMD ["serve", "--config", "/etc/wye/wye.toml"]
