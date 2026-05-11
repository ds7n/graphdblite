# graphdblite Go binding

User-facing docs: [../../docs/go.md](../../docs/go.md). This file covers
the binding's *build layout* — what lives where and how `go get` is
expected to work end-to-end.

## Layout

```
bindings/go/
├── graphdblite.go        # cgo wrapper around libgraphdblite_ffi
├── Makefile              # local dev builds (detects host triple)
└── lib/
    ├── linux_amd64/      # libgraphdblite_ffi.a (committed at release time)
    ├── linux_arm64/
    ├── darwin_arm64/
    └── windows_amd64/
```

## How cgo finds the static lib

`graphdblite.go` carries per-platform `#cgo` directives:

```c
#cgo linux,amd64   LDFLAGS: -L${SRCDIR}/lib/linux_amd64   -lgraphdblite_ffi -lm -ldl -lpthread
#cgo linux,arm64   LDFLAGS: -L${SRCDIR}/lib/linux_arm64   -lgraphdblite_ffi -lm -ldl -lpthread
#cgo darwin,arm64  LDFLAGS: -L${SRCDIR}/lib/darwin_arm64  -lgraphdblite_ffi -lm
#cgo windows,amd64 LDFLAGS: -L${SRCDIR}/lib/windows_amd64 -lgraphdblite_ffi -lws2_32 -luserenv -lntdll -ladvapi32 -lbcrypt
```

`${SRCDIR}` resolves to this directory at cgo-compile time, so the
toolchain picks the matching `lib/<os_arch>/libgraphdblite_ffi.a` for
the host. This is what makes `go get
github.com/ds7n/graphdblite/bindings/go` work without a Rust toolchain
on the consumer's machine.

## Local development (`make build`)

`make build` detects the host triple via `uname` and drops the freshly
compiled `libgraphdblite_ffi.{a,so,dylib}` into `lib/<os_arch>/`. The
flat `lib/*.a` is `.gitignore`'d so a local rebuild never pollutes the
tracked tree; the committed `lib/<os_arch>/libgraphdblite_ffi.a` files
are protected by `!lib/<os_arch>/` rules in `.gitignore`.

## Release lib population

At release time, `scripts/populate-go-libs.sh` (TODO) downloads the
four `graphdblite-ffi-<triple>.tar.gz` archives from the GitHub release
and unpacks each one into the right `lib/<os_arch>/` subdir, then
stages them for the release commit (`git add -f`). The full release
sequence lives in [`docs/publishing.md`](../../docs/publishing.md).

## Supported platforms

| `GOOS` | `GOARCH` | Status |
|---|---|---|
| `linux`   | `amd64` | supported |
| `linux`   | `arm64` | supported |
| `darwin`  | `arm64` | supported (Apple Silicon only) |
| `windows` | `amd64` | supported (uses MinGW-built FFI, `x86_64-pc-windows-gnu`) |

Out of scope for now: Intel Macs (`darwin/amd64`), 32-bit Linux,
`armv7`. See [TODO.md](../../TODO.md) for the rationale and what'd be
required to add them.
