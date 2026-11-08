# Native Rust API — default production image.
FROM rust:1-bookworm AS builder
WORKDIR /build
ARG RUSTFLAGS="-D warnings"
ARG MATCHLAB_BUILD_SHA=""
ARG MATCHLAB_FEATURES=""
ENV MATCHLAB_BUILD_SHA=$MATCHLAB_BUILD_SHA
ENV RUSTFLAGS=$RUSTFLAGS
COPY . .
RUN if [ -n "$MATCHLAB_FEATURES" ]; then cargo build --release -p matchlab-server --features "$MATCHLAB_FEATURES"; \
    else cargo build --release -p matchlab-server; fi

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tini curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home app \
    && mkdir -p /data && chown app:app /data
COPY --from=builder /build/target/release/matchlab-server /usr/local/bin/matchlab-server
USER app
ENV MATCHLAB_DB=/data/metadata.sqlite3 MATCHLAB_HOST=0.0.0.0:8765
VOLUME ["/data"]
EXPOSE 8765
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD ["curl", "--fail", "--silent", "http://127.0.0.1:8765/health/live"]
ENTRYPOINT ["/usr/bin/tini", "-g", "--"]
CMD ["/usr/local/bin/matchlab-server"]
