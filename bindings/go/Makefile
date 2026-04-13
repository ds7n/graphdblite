# Build the Rust C FFI library, then run Go tests.
.PHONY: build test clean

REPO_ROOT := $(shell git -C $(dir $(lastword $(MAKEFILE_LIST))) rev-parse --show-toplevel)
LIB_DIR := $(CURDIR)/lib

build:
	cargo build --release -p graphdblite-ffi --manifest-path $(REPO_ROOT)/Cargo.toml
	mkdir -p $(LIB_DIR)
	cp $(REPO_ROOT)/target/release/libgraphdblite_ffi.a $(LIB_DIR)/ 2>/dev/null || \
	cp $(REPO_ROOT)/target/release/libgraphdblite_ffi.so $(LIB_DIR)/ 2>/dev/null || \
	cp $(REPO_ROOT)/target/release/libgraphdblite_ffi.dylib $(LIB_DIR)/ 2>/dev/null || true

test: build
	CGO_LDFLAGS="-L$(LIB_DIR) -lgraphdblite_ffi -lm -ldl -lpthread" go test -v ./...

clean:
	rm -rf $(LIB_DIR)
