# Build the Rust C FFI library, then run Go tests.
#
# The committed lib/ tree has one subdirectory per supported platform
# (lib/linux_amd64, lib/linux_arm64, lib/darwin_arm64, lib/windows_amd64);
# `make build` detects the host triple and drops the freshly-built
# `libgraphdblite_ffi.a` into the matching subdirectory so cgo can find
# it via the per-platform `#cgo … LDFLAGS` in graphdblite.go.
.PHONY: build test clean

REPO_ROOT := $(shell git -C $(dir $(lastword $(MAKEFILE_LIST))) rev-parse --show-toplevel)
LIB_ROOT  := $(CURDIR)/lib

# Map `uname` output → the os_arch directory layout used in lib/.
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)

ifeq ($(UNAME_S),Linux)
  OS := linux
else ifeq ($(UNAME_S),Darwin)
  OS := darwin
else
  $(error Unsupported host OS: $(UNAME_S))
endif

ifeq ($(UNAME_M),x86_64)
  ARCH := amd64
else ifeq ($(UNAME_M),aarch64)
  ARCH := arm64
else ifeq ($(UNAME_M),arm64)
  ARCH := arm64
else
  $(error Unsupported host arch: $(UNAME_M))
endif

PLATFORM_DIR := $(LIB_ROOT)/$(OS)_$(ARCH)

build:
	cargo build --release -p graphdblite-ffi --manifest-path $(REPO_ROOT)/Cargo.toml
	mkdir -p $(PLATFORM_DIR)
	cp $(REPO_ROOT)/target/release/libgraphdblite_ffi.a $(PLATFORM_DIR)/ 2>/dev/null || \
	cp $(REPO_ROOT)/target/release/libgraphdblite_ffi.so $(PLATFORM_DIR)/ 2>/dev/null || \
	cp $(REPO_ROOT)/target/release/libgraphdblite_ffi.dylib $(PLATFORM_DIR)/ 2>/dev/null || true

test: build
	CGO_LDFLAGS="-L$(PLATFORM_DIR) -lgraphdblite_ffi -lm -ldl -lpthread" go test -v ./...

clean:
	find $(LIB_ROOT) -mindepth 2 -type f ! -name .gitkeep -delete
