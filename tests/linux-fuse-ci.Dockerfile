FROM rust:1.96.0-bookworm@sha256:5e2214abe154fe26e39f64488952e5c991eeed1d6d6da7cc8381ae83927f0cfc

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
      ca-certificates \
      curl \
      fuse3 \
      git \
      jq \
      libfuse3-dev \
      pkg-config \
      procps \
      python3 \
      sqlite3 \
      util-linux \
    && chmod 4755 /usr/bin/fusermount3 \
    && install -d -m 1777 \
      /tmp/locality-cargo \
      /tmp/locality-home \
      /tmp/locality-target \
    && rm -rf /var/lib/apt/lists/*

COPY linux-fuse-ci-entrypoint.sh /usr/local/bin/locality-fuse-ci-entrypoint
RUN chmod 0755 /usr/local/bin/locality-fuse-ci-entrypoint

ENV CARGO_HOME=/tmp/locality-cargo
ENV CARGO_TARGET_DIR=/tmp/locality-target
ENV HOME=/tmp/locality-home
ENV RUSTUP_HOME=/usr/local/rustup
ENV RUSTUP_TOOLCHAIN=1.96.0

ENTRYPOINT ["/usr/local/bin/locality-fuse-ci-entrypoint"]
