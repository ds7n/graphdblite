# Build the C FFI library and the conformance harness.
.PHONY: build conformance clean

REPO_ROOT := $(shell git -C $(dir $(lastword $(MAKEFILE_LIST))) rev-parse --show-toplevel)
LIB_DIR := $(REPO_ROOT)/target/release
CFLAGS := -std=c11 -Wall -Wextra -O2
LDFLAGS := -L$(LIB_DIR) -Wl,-rpath,$(LIB_DIR) -lgraphdblite_ffi -lm -ldl -lpthread

build:
	cargo build --release -p graphdblite-ffi --manifest-path $(REPO_ROOT)/Cargo.toml

conformance: build
	$(CC) $(CFLAGS) tests/conformance.c -o tests/conformance $(LDFLAGS)
	./tests/conformance

clean:
	rm -f tests/conformance
