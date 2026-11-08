# MatchLab single production image: Rust parser always works, while
# ONNX Runtime and the tiny ONNX model are bundled but NEVER initialized
# unless MATCHLAB_ML_ENABLED=true.
# Multi-arch: linux/amd64 and linux/arm64; independently pinned archives.
FROM rust:1-bookworm AS builder
WORKDIR /build
ARG TARGETARCH
ARG RUSTFLAGS="-D warnings"
ARG MATCHLAB_BUILD_SHA=""
ENV MATCHLAB_BUILD_SHA=$MATCHLAB_BUILD_SHA
ENV RUSTFLAGS=$RUSTFLAGS
COPY . .
# Default production image includes Rust ONNX support; feature compilation
# does not create a worker or load ONNX Runtime when ML is disabled.
RUN cargo build --release -p matchlab-server --features ml-ort
# Generate the tiny, deterministic model from Rust in the build stage.
RUN mkdir -p /build/ml && /build/target/release/matchlab-server emit-ml-model /build/ml/tiny-anomaly.onnx
# Official Microsoft CPU-only ONNX Runtime 1.30.0 for both supported arches.
# Verify downloaded contents against the SHA-256 values of GitHub release
# assets. No network access required at container runtime.
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) ORT_ARCH=x64; ORT_SHA=a5ed5a3cac51fbb2e90da632ae43d19212faaa20e76484e62bcb7c23ddb3b3fd ;; \
      arm64) ORT_ARCH=aarch64; ORT_SHA=e16a27a8ed330bbc698df7330b0cf56e722f354e3bcc92118682c74ef3c3e3da ;; \
      *) echo "Unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fSL --retry 3 --retry-delay 2 \
      "https://github.com/microsoft/onnxruntime/releases/download/v1.30.0/onnxruntime-linux-${ORT_ARCH}-1.30.0.tgz" \
      -o /tmp/onnxruntime.tgz; \
    echo "${ORT_SHA}  /tmp/onnxruntime.tgz" | sha256sum -c -; \
    mkdir -p /tmp/onnxruntime; \
    tar -xzf /tmp/onnxruntime.tgz -C /tmp/onnxruntime; \
    cp "/tmp/onnxruntime/onnxruntime-linux-${ORT_ARCH}-1.30.0/lib/libonnxruntime.so.1.30.0" /build/ml/; \
    chmod 755 /build/ml/libonnxruntime.so.1.30.0; \
    rm -rf /tmp/onnxruntime /tmp/onnxruntime.tgz

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tini curl libstdc++6 libgomp1 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home app \
    && mkdir -p /data /opt/matchlab/ml \
    && chown app:app /data
COPY --from=builder /build/target/release/matchlab-server /usr/local/bin/matchlab-server
COPY --from=builder /build/ml/ /opt/matchlab/ml/
USER app
ENV MATCHLAB_DB=/data/metadata.sqlite3 \
    MATCHLAB_HOST=0.0.0.0:8765 \
    MATCHLAB_ML_ENABLED=false \
    ORT_DYLIB_PATH=/opt/matchlab/ml/libonnxruntime.so.1.30.0 \
    MATCHLAB_ML_MODEL_PATH=/opt/matchlab/ml/tiny-anomaly.onnx
VOLUME ["/data"]
EXPOSE 8765
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD ["curl", "--fail", "--silent", "http://127.0.0.1:8765/health/live"]
ENTRYPOINT ["/usr/bin/tini", "-g", "--"]
CMD ["/usr/local/bin/matchlab-server"]
