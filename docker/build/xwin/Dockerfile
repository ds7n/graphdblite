FROM rust:1-bookworm

# LLVM apt repository
RUN apt-get update && apt-get install -y wget gnupg \
    && wget -qO- https://apt.llvm.org/llvm-snapshot.gpg.key | tee /etc/apt/trusted.gpg.d/apt.llvm.org.asc \
    && echo "deb http://apt.llvm.org/bookworm/ llvm-toolchain-bookworm-18 main" \
       > /etc/apt/sources.list.d/llvm.list

# LLVM tools + Python
# clang-18 includes clang-cl-18; lld-18 includes lld-link-18
RUN apt-get update && apt-get install -y \
    clang-18 lld-18 llvm-18 zip \
    python3 python3-pip python3-venv \
    && rm -rf /var/lib/apt/lists/*

# Create unversioned symlinks for all LLVM tools that cargo-xwin/PyO3 expect.
# Some tools are at /usr/bin/tool-18, others at /usr/lib/llvm-18/bin/tool.
RUN for tool in \
      clang clang++ clang-cl \
      lld lld-link ld.lld \
      llvm-lib llvm-dlltool llvm-ar llvm-ranlib llvm-objcopy llvm-strip llvm-nm; do \
      if [ -f "/usr/bin/${tool}-18" ]; then \
        ln -sf "/usr/bin/${tool}-18" "/usr/local/bin/${tool}"; \
      elif [ -f "/usr/lib/llvm-18/bin/${tool}" ]; then \
        ln -sf "/usr/lib/llvm-18/bin/${tool}" "/usr/local/bin/${tool}"; \
      fi; \
    done

# Node.js 20
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# cargo-xwin + maturin
RUN cargo install cargo-xwin --locked \
    && pip3 install --break-system-packages maturin

# Rust target
RUN rustup target add x86_64-pc-windows-msvc

WORKDIR /build
COPY build.sh /build.sh
RUN chmod +x /build.sh
ENTRYPOINT ["/build.sh"]
