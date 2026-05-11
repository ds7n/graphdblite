FROM rust:1-bookworm

# Zig (for cross-compilation)
RUN curl -fsSL https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz \
    | tar -C /usr/local -xJ \
    && ln -s /usr/local/zig-linux-x86_64-0.13.0/zig /usr/local/bin/zig

# Node.js 20 + zip (for packaging windows-gnu artifacts)
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y nodejs zip \
    && rm -rf /var/lib/apt/lists/*

# Rust targets:
#   - aarch64-unknown-linux-gnu — Linux arm64 (CLI, FFI, Node)
#   - x86_64-pc-windows-gnu     — Windows MinGW (FFI only, for Go cgo)
RUN rustup target add aarch64-unknown-linux-gnu x86_64-pc-windows-gnu

WORKDIR /build
COPY build.sh /build.sh
RUN chmod +x /build.sh
ENTRYPOINT ["/build.sh"]
