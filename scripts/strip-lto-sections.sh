#!/usr/bin/env bash
#
# Remove LLVM bitcode sections from a static library so Go's cgo accepts it.
#
# Go 1.26.11+ (cmd/go/internal/work/security.go) rejects linker inputs that
# carry LTO sections: `.llvmbc` / `.llvmcmd` (ELF, COFF) and the `__LLVM`
# segment (Mach-O). Rust's precompiled std objects embed bitcode, so every
# Rust staticlib has them regardless of build flags. Removing them changes no
# machine code: the sections are only an LTO input that the Go link ignores.
#
# Usage: scripts/strip-lto-sections.sh path/to/libgraphdblite_ffi.a
#
# Uses llvm-objcopy (handles ELF, Mach-O and COFF for any architecture) from
# `rustup component add llvm-tools`, else `llvm-objcopy` on PATH, else GNU
# objcopy (host ELF only). Fails if any disallowed section remains.
set -euo pipefail

if [ $# -ne 1 ] || [ ! -f "$1" ]; then
    echo "usage: $0 <static-library.a>" >&2
    exit 2
fi
lib="$1"

sysroot_bin="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"
objcopy=""
objdump=""
for dir in "$sysroot_bin" ""; do
    oc="${dir:+$dir/}llvm-objcopy"
    od="${dir:+$dir/}llvm-objdump"
    if command -v "$oc" >/dev/null 2>&1 && "$oc" --version >/dev/null 2>&1; then
        objcopy="$oc"
        objdump="$od"
        break
    fi
done
if [ -z "$objcopy" ]; then
    if command -v objcopy >/dev/null 2>&1; then
        objcopy="objcopy"
        objdump="objdump"
    else
        echo "error: no llvm-objcopy or objcopy found (try: rustup component add llvm-tools)" >&2
        exit 1
    fi
fi

"$objcopy" \
    --remove-section=.llvmbc \
    --remove-section=.llvmcmd \
    --remove-section=__LLVM,__bitcode \
    --remove-section=__LLVM,__cmdline \
    --remove-section=__LLVM,__asm \
    "$lib"

# Verify: list section names of every member and look for leftovers.
if "$objdump" -h "$lib" 2>/dev/null | grep -qE '\.llvmbc|\.llvmcmd|__LLVM|__bitcode|\.gnu\.lto_|\.llvm\.lto'; then
    echo "error: LTO sections remain in $lib" >&2
    "$objdump" -h "$lib" | grep -E '\.llvmbc|\.llvmcmd|__LLVM|__bitcode|\.gnu\.lto_|\.llvm\.lto' | head -5 >&2
    exit 1
fi
echo "stripped LTO sections: $lib ($objcopy)"
