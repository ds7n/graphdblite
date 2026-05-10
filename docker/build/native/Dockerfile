FROM rust:1-bookworm

# Node.js 20
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y nodejs zip \
    && rm -rf /var/lib/apt/lists/*

# Go (for binding tests)
RUN curl -fsSL https://go.dev/dl/go1.22.5.linux-amd64.tar.gz | tar -C /usr/local -xz
ENV PATH="/usr/local/go/bin:${PATH}"

WORKDIR /build
COPY build.sh /build.sh
RUN chmod +x /build.sh
ENTRYPOINT ["/build.sh"]
