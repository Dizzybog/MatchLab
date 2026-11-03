# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-dev python3-pip python3-venv pkg-config build-essential patchelf && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY . .
RUN python3 -m venv /venv && /venv/bin/pip install --no-cache-dir maturin
RUN /venv/bin/maturin build --release --interpreter /usr/bin/python3 --out /dist

FROM python:3.11-slim-bookworm
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1
RUN useradd --system --uid 10001 --create-home app && mkdir -p /data && chown -R app:app /data
COPY --from=builder /dist/*.whl /tmp/
RUN pip install --no-cache-dir /tmp/*.whl && rm -f /tmp/*.whl
USER app
VOLUME ["/data"]
EXPOSE 8765
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 CMD python -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8765/health', timeout=3)"
CMD ["media-match", "--db", "/data/metadata.sqlite3", "service", "--host", "0.0.0.0", "--port", "8765"]
