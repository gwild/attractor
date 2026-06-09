# Pinned Rust 1.92 build/test sandbox for portable Attractor crates
# (attractor-core, -proto, -config, -api, -io). Issue #2288 / WS-5.
#
# Exact minor is load-bearing: attest.sh and run.sh tag this image
# attractor-build:1.92 and use that tag as the rebuild cache key.
FROM rust:1.92-slim-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        build-essential \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --uid 1000 --create-home --shell /bin/bash app

ENV CARGO_HOME=/home/app/.cargo
RUN mkdir -p /home/app/.cargo/registry /home/app/.cargo/git && chown -R app:app /home/app/.cargo

WORKDIR /work
USER app

CMD ["rustc", "--version"]
