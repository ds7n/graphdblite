FROM rust:1-bookworm

# Build dependencies for osxcross
RUN apt-get update && apt-get install -y \
    cmake clang llvm-dev libxml2-dev uuid-dev libssl-dev \
    python3 python3-pip python3-venv patch xz-utils bzip2 cpio zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

# Node.js 20
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# osxcross — build from source
# The macOS SDK tarball must be provided at build time or mounted at runtime.
# To build: extract MacOSX14.0.sdk from Xcode, then:
#   tar -cJf MacOSX14.0.sdk.tar.xz MacOSX14.0.sdk
#   docker build --build-arg SDK=MacOSX14.0.sdk.tar.xz -t graphdblite-macos docker/macos/
ARG SDK=MacOSX14.0.sdk.tar.xz
RUN git clone --depth 1 https://github.com/tpoechtrager/osxcross /opt/osxcross
COPY ${SDK} /opt/osxcross/tarballs/
RUN cd /opt/osxcross && UNATTENDED=1 ./build.sh
ENV PATH="/opt/osxcross/target/bin:${PATH}"

# maturin
RUN pip3 install --break-system-packages maturin

# Rust targets
RUN rustup target add aarch64-apple-darwin x86_64-apple-darwin

WORKDIR /build
COPY build.sh /build.sh
RUN chmod +x /build.sh
ENTRYPOINT ["/build.sh"]
