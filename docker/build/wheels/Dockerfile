# PyPA manylinux/musllinux base — provides multiple Python interpreters at
# /opt/python/cp*/bin and the right glibc/musl floor for portable wheels.
# Pinned via BASE_IMAGE so the same Dockerfile produces glibc and musl wheels,
# x86_64 and aarch64 (the latter via qemu/binfmt when run on x86_64 hosts).
ARG BASE_IMAGE
FROM ${BASE_IMAGE}

# Rust toolchain — rustup is self-contained, doesn't need yum/apk.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

# maturin runs from any of the bundled Pythons; pick cp311 as a stable choice.
# The wheels themselves are built abi3-compatible per pyproject.toml.
RUN /opt/python/cp311-cp311/bin/pip install --no-cache-dir maturin

WORKDIR /build
COPY build.sh /build.sh
RUN chmod +x /build.sh
ENTRYPOINT ["/build.sh"]
