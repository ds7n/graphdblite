FROM rust:1-bookworm

# Python + maturin
RUN apt-get update && apt-get install -y python3 python3-pip python3-venv \
    && rm -rf /var/lib/apt/lists/* \
    && pip3 install --break-system-packages maturin

# Zig
RUN curl -fsSL https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz \
    | tar -C /usr/local -xJ \
    && ln -s /usr/local/zig-linux-x86_64-0.13.0/zig /usr/local/bin/zig

# Rust targets
RUN rustup target add \
    x86_64-unknown-linux-gnu \
    x86_64-unknown-linux-musl \
    aarch64-unknown-linux-gnu \
    aarch64-unknown-linux-musl

WORKDIR /build
COPY build.sh /build.sh
RUN chmod +x /build.sh
ENTRYPOINT ["/build.sh"]
