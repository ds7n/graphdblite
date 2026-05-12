# Publishing graphdblite

End-to-end release procedure. graphdblite ships to five places:

| Target | What | How |
|---|---|---|
| **crates.io** | `graphdblite` (Rust library + `graphdblite` CLI) | `cargo publish` |
| **PyPI** | `graphdblite` (Python wheels) | GitHub Actions → `pypa/gh-action-pypi-publish` |
| **npm** | `graphdblite` (Node.js addon) | `npm publish` from `bindings/node/` |
| **pkg.go.dev** | `github.com/ds7n/graphdblite/bindings/go` | nothing — auto-indexed from git tags |
| **GitHub / Forgejo releases** | CLI binaries, `libgraphdblite_ffi.{so,a,dll}`, prebuilt wheels, `.node` addons | `scripts/publish-release.sh` |

The `graphdblite` name has to be claimed on PyPI / crates.io / npm before the first release. After that, each registry is independent — you can ship a Python wheel without re-publishing the Rust crate.

---

## Pre-flight checklist

Before any release:

1. **Bump versions in lockstep**:
   - `Cargo.toml` `[workspace.package] version = "X.Y.Z"` (all crates inherit).
   - `bindings/node/package.json` `version`.
   - `bindings/python/pyproject.toml` — version comes from Cargo via `dynamic = ["version"]` + maturin, so no change needed.
   - `bindings/go/` — Go modules version by git tag only.
2. **Update `CHANGELOG.md`** — move entries under the new version heading.
3. **Run the full test gate**:
   ```bash
   cargo test --tests
   cargo test --test tck --features tck-support
   cargo test --test record_columns_golden --features tck-support
   cargo test --test alloc_regression --features dhat-heap --release
   ```
4. **Commit + tag**:
   ```bash
   git commit -am "release: vX.Y.Z"
   git tag vX.Y.Z
   ```
5. **Push the tag** (forgejo by default, github when ready):
   ```bash
   git push                  # origin/forgejo
   git push github vX.Y.Z    # github mirror
   ```

---

## crates.io (Rust)

Only the root `graphdblite` crate publishes. `bindings/{ffi,node,python}` and
`benches-crate` are all `publish = false`.

**One-time setup:**

1. Create an account at <https://crates.io>.
2. Generate an API token: <https://crates.io/settings/tokens>.
3. `cargo login <token>` (stored in `~/.cargo/credentials.toml`).
4. Reserve the name with a `0.0.0-reserved` placeholder if you don't have it yet.

**Required metadata in `Cargo.toml`** (workspace section):

```toml
[workspace.package]
license = "MIT"                                # already set
description = "..."                            # already set
repository = "https://github.com/ds7n/graphdblite"
homepage = "https://github.com/ds7n/graphdblite"
documentation = "https://docs.rs/graphdblite"
readme = "README.md"
keywords = ["graph", "database", "cypher", "embedded", "sqlite"]
categories = ["database", "database-implementations"]
```

(`keywords` max 5, `categories` from <https://crates.io/category_slugs>.)

**Each release:**

```bash
# 1. Dry-run — verifies the package builds in isolation, no upload.
cargo publish --dry-run -p graphdblite

# 2. Real publish.
cargo publish -p graphdblite
```

`docs.rs` rebuilds the docs automatically within ~30 min. Yanking a release:
`cargo yank --version X.Y.Z`.

---

## PyPI (Python wheels)

**Automated** via `.github/workflows/python-wheels.yml`.

**One-time setup:**

1. Create accounts on <https://pypi.org> and <https://test.pypi.org>.
2. Configure PyPI **trusted publishing** (no API token needed):
   - <https://pypi.org/manage/account/publishing/> → Add pending publisher.
   - Repository owner: `ds7n`, name: `graphdblite`, workflow: `python-wheels.yml`,
     environment: `pypi`.
3. Add a GitHub environment named `pypi` (Settings → Environments) — gates the
   publish step on manual approval if you want.

**Each release:**

The workflow triggers on tag push (`v*`). It builds wheels for:

- Linux glibc + musl (x86_64, aarch64) via `cibuildwheel`
- macOS (x86_64, arm64)
- Windows (x86_64)
- abi3 wheels (Python 3.10+)

Then uploads via `pypa/gh-action-pypi-publish@release/v1`.

**Manual fallback** (if the workflow can't run):

```bash
cd bindings/python
uv tool install maturin
maturin build --release --strip --features abi3
maturin publish --username __token__ --password <pypi-token>
```

**Required metadata** in `bindings/python/pyproject.toml`:

```toml
[project]
name = "graphdblite"
description = "Embedded graph database with Cypher support"
license = "MIT"
license-files = ["LICENSE"]
authors = [{ name = "ds7n" }]
requires-python = ">=3.10"
classifiers = [
    "Development Status :: 3 - Alpha",
    "License :: OSI Approved :: MIT License",
    "Programming Language :: Python :: 3",
    "Programming Language :: Rust",
    "Topic :: Database :: Database Engines/Servers",
]

[project.urls]
Repository = "https://github.com/ds7n/graphdblite"
Documentation = "https://github.com/ds7n/graphdblite/blob/main/docs/python.md"
```

---

## npm (Node.js addon)

**Not yet automated** — manual `napi` publish flow.

**One-time setup:**

1. Create an account at <https://www.npmjs.com>.
2. `npm login` (or use a granular access token: `npm config set //registry.npmjs.org/:_authToken <token>`).
3. Reserve the `graphdblite` name with a `0.0.1-prereserve` placeholder if needed.

**Each release:**

`@napi-rs/cli` builds platform-specific addons and a JS wrapper that picks the
right one at install time.

```bash
cd bindings/node

# 1. Build the .node addon for every triple listed in package.json
#    (currently x86_64/aarch64 linux, x86_64/aarch64 darwin, x86_64 windows).
#    This requires cross-compilation toolchains — easiest to run per-OS in CI.
npm install
npm run build -- --release

# 2. Publish the main package + one platform package per triple.
npx napi prepublish -t github          # generates per-platform package.json files
npm publish --access public            # main package
# Then for each platform sub-package (npm/@napi-rs publishes them as
# graphdblite-linux-x64-gnu, graphdblite-darwin-arm64, etc.):
(cd npm/linux-x64-gnu && npm publish --access public)
# ... repeat for each platform directory
```

**Required metadata** in `bindings/node/package.json` (additions):

```json
{
  "repository": { "type": "git", "url": "git+https://github.com/ds7n/graphdblite.git" },
  "author": "ds7n",
  "keywords": ["graph", "database", "cypher", "embedded", "sqlite"],
  "homepage": "https://github.com/ds7n/graphdblite#readme",
  "bugs": { "url": "https://github.com/ds7n/graphdblite/issues" }
}
```

**Future work:** an `.github/workflows/npm-publish.yml` that builds on each
triple's native runner and pushes to npm via `NPM_TOKEN` secret would let
this step ride the tag-push trigger like the wheels workflow does.

---

## pkg.go.dev (Go module)

**No publish step.** Go modules are discovered from git tags. But `go get`
will fail unless the per-platform `libgraphdblite_ffi.a` archives are
*committed* under the tag — see "Go binding lib population" below.

After pushing `vX.Y.Z-go` to the GitHub mirror (`git push github vX.Y.Z-go`),
`pkg.go.dev` indexes the module within a few minutes. Trigger indexing manually
by visiting `https://pkg.go.dev/github.com/ds7n/graphdblite/bindings/go@vX.Y.Z-go`.

**Caveat:** Go modules versioned at v2 or higher require a `/vN` suffix on the
import path. Until then, stay on `v0.*` / `v1.*`.

The `bindings/go/LICENSE` file (copy of root LICENSE) is what `pkg.go.dev` reads
for the license badge.

### Go binding lib population

The Go binding statically links a per-platform `libgraphdblite_ffi.a` via cgo.
`bindings/go/lib/<os_arch>/libgraphdblite_ffi.a` files are gitignored in normal
development (only `.gitkeep` placeholders are tracked) so the working tree
stays lean. **For `go get` to work on a clean machine with no Rust toolchain,
those archives must be present in the tag a Go consumer pulls.**

Sequence:

1. Tag `vX.Y.Z` and push to both remotes (this triggers the FFI builds):
   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   git push github vX.Y.Z
   ```
2. Wait for CI (`.github/workflows/dev-build.yml` → `build-ffi` job) to attach
   the four FFI archives to the GitHub release:
   - `graphdblite-ffi-x86_64-unknown-linux-gnu.tar.gz`
   - `graphdblite-ffi-aarch64-unknown-linux-gnu.tar.gz`
   - `graphdblite-ffi-aarch64-apple-darwin.tar.gz`
   - `graphdblite-ffi-x86_64-pc-windows-gnu.zip` (MinGW — Go cgo needs `.a`)
3. Run the populate script locally:
   ```bash
   scripts/populate-go-libs.sh vX.Y.Z              # pulls from forgejo
   scripts/populate-go-libs.sh --github vX.Y.Z     # pulls from github
   ```
   It extracts each archive's `libgraphdblite_ffi.a` into
   `bindings/go/lib/<os_arch>/` and force-stages the files
   (they're gitignored, so `git add -f` is required).
4. Commit + tag with a `-go` suffix so the lib-bearing commit has its own tag
   distinct from `vX.Y.Z`:
   ```bash
   git commit -m "release: vX.Y.Z Go FFI libs"
   git tag -a vX.Y.Z-go -m "Go FFI libs for vX.Y.Z"
   git push origin vX.Y.Z-go
   git push github vX.Y.Z-go
   ```
5. Users `go get github.com/ds7n/graphdblite/bindings/go@vX.Y.Z-go`.

**Module weight.** Each `libgraphdblite_ffi.a` is ~35 MB, so the four
platforms add ~140 MB to the committed module. This is acceptable as long as
the main repo stays usable; if module size becomes a problem, consider hosting
the libs in a separate `graphdblite-go-libs` repo and pulling them via
`go:generate`, or moving them to `git-lfs` (cgo still resolves LFS-fetched
files at build time).

**Env overrides** (populate-go-libs.sh and publish-release.sh both
source `.env` at the repo root if present):

```bash
FORGEJO_URL=https://forgejo.example.com \
FORGEJO_OWNER=your-org FORGEJO_REPO=graphdblite \
GITHUB_OWNER=ds7n GITHUB_REPO=graphdblite \
  scripts/populate-go-libs.sh vX.Y.Z
```

**Automated path.** `.github/workflows/release-go.yml` runs on
`release: published` (and via manual `workflow_dispatch` with a tag
input). It downloads the FFI assets from the just-published GitHub
release, runs `populate-go-libs.sh --github`, commits on a detached
HEAD off the released tag's commit, and pushes a parallel `vX.Y.Z-go`
tag. The commit is *not* pushed to main — only the tag.

Forgejo doesn't run GitHub Actions, so the `-go` tag reaches forgejo
via mirror push (`git push origin vX.Y.Z-go` from a clone that already
has the tag fetched from github), or run the populate flow manually
against forgejo:

```bash
scripts/populate-go-libs.sh vX.Y.Z   # forgejo default
git commit -m "release: vX.Y.Z Go FFI libs"
git tag -a vX.Y.Z-go -m "Go FFI libs for vX.Y.Z"
git push origin vX.Y.Z-go
```

---

## GitHub / Forgejo releases (binary artifacts)

**Automated** via `scripts/publish-release.sh` and the `just publish*` targets.

These releases host pre-built binaries for users who don't want to compile:

- `graphdblite` CLI: Linux x86_64/aarch64, Windows x86_64 (macOS via the
  optional `.github/workflows/release.yml`, currently disabled)
- `libgraphdblite_ffi.{so,a,dll}` + `graphdblite.h`
- `.node` addons (one per triple)
- Python `.whl` files (also pushed to PyPI separately)

**Each release:**

```bash
# 1. Build all artifacts into dist/ (uses cross-rs for non-host triples).
just build-release

# 2. Publish to Forgejo (default — uses FORGEJO_TOKEN from .env).
just publish vX.Y.Z

# 3. Also publish to GitHub.
just publish-github vX.Y.Z
```

**Rolling dev build** from `main`:

```bash
just publish-dev          # forgejo dev-latest prerelease
just publish-github-dev   # github dev-latest prerelease
```

Both are overwritten on each invocation. `dist/sha256sums.txt` is regenerated
and uploaded alongside artifacts.

---

## Order of operations for a clean release

1. Bump versions + CHANGELOG, commit.
2. Tag `vX.Y.Z`, push to both remotes.
3. **crates.io**: `cargo publish -p graphdblite`
4. **GitHub Actions**: tag push triggers `python-wheels.yml` → PyPI upload.
5. **npm**: build + publish from `bindings/node/` (manual until automated).
6. **Forgejo + GitHub releases**: `just publish vX.Y.Z && just publish-github vX.Y.Z`
7. **Go FFI libs (GitHub)**: automatic — `.github/workflows/release-go.yml`
   fires on `release: published`, populates libs, pushes `vX.Y.Z-go`.
   For forgejo, also run `scripts/populate-go-libs.sh vX.Y.Z` + commit +
   tag locally. See "Go binding lib population" above.
8. **pkg.go.dev**: nothing — happens automatically once the `vX.Y.Z-go` tag is
   visible on GitHub.

Verify each landed:

- <https://crates.io/crates/graphdblite>
- <https://pypi.org/project/graphdblite/>
- <https://www.npmjs.com/package/graphdblite>
- <https://pkg.go.dev/github.com/ds7n/graphdblite/bindings/go>
- <https://github.com/ds7n/graphdblite/releases> (and Forgejo equivalent)
