FROM rust:1-bookworm

# Zig (for cross-compilation)
RUN curl -fsSL https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz \
    | tar -C /usr/local -xJ \
    && ln -s /usr/local/zig-linux-x86_64-0.13.0/zig /usr/local/bin/zig

# Node.js 20
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# Rust target
RUN rustup target add aarch64-unknown-linux-gnu

WORKDIR /build
COPY build.sh /build.sh
RUN chmod +x /build.sh
ENTRYPOINT ["/build.sh"]
