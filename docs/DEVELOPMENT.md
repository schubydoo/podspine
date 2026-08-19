# Development

Setting up, building, and testing Podspine locally. For the contribution workflow
(branching, commits, PRs), see [CONTRIBUTING.md](https://github.com/schubydoo/podspine/blob/main/CONTRIBUTING.md); for how the
system fits together, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Prerequisites

- A stable **Rust** toolchain. `rust-toolchain.toml` pins the `stable` channel with
  `rustfmt` + `clippy`; the workspace's minimum is `rust-version = 1.88` (edition
  2024).
- **`ffmpeg` and `ffprobe` on your `PATH`.** The prober/splitter shell out to them,
  and many tests synthesize fixtures with `ffmpeg`.

## Workspace layout

A Cargo workspace, one crate per pipeline stage (see [ARCHITECTURE.md](ARCHITECTURE.md)
for what each does):

```
Cargo.toml            # workspace + the `podspine` server binary
src/main.rs           # server entrypoint: config → scan → watch → serve
crates/
├── config            # CLI/env/TOML resolution + ffmpeg preflight
├── scanner           # library walk + per-book orchestration
├── prober            # ffprobe wrapper
├── chapters          # sidecar (.cue/.ffmeta) vs embedded resolution
├── splitter          # ffmpeg stream-copy + cover extraction
├── index             # rusqlite (bundled) store
├── feed              # RSS 2.0 + itunes/podcast + self-check
├── http              # Axum router + middleware
├── ui                # maud pages
├── metrics           # optional Prometheus instrumentation (--metrics-bind)
└── test-support      # dev-only test fixtures (skip! macros, ffmpeg synthesis)
```

## Common commands

```bash
cargo build                                   # build the workspace
cargo run -- --library ./sample-books         # run the server → http://localhost:8080
cargo test --workspace                        # run all tests
cargo clippy --workspace --all-targets --all-features -- -D warnings  # lint (matches CI's gate)
cargo fmt                                      # format
```

(`./sample-books` is just an example path — point `--library` at any folder of
audiobooks.)

## Tests

- Pure logic (feed generation, chapter/cue parsing, slug rules, config resolution,
  MIME mapping, the path-traversal allow-list) is unit-tested without any external
  process.
- Many integration-style tests **synthesize fixtures with `ffmpeg`** (a short sine
  tone, embedded chapters, an attached cover, real MP3/FLAC/Opus files) and then run
  the pipeline. These are gated on tool availability: if `ffmpeg`/`ffprobe` — or a
  specific encoder — isn't present, the test prints a skip notice and returns rather
  than failing. Encoders used include `aac`, `libmp3lame`, `flac`, and `libopus`.
  The shared machinery (the `skip!`/`skip_unless_ffmpeg!` macros, fixture synthesis,
  scratch-dir guards) lives in the dev-only `crates/test-support` crate. CI runs the
  suite through `cargo nextest run`; plain `cargo test` and local `cargo nextest run`
  both work.

Run with logs to see scan/serve behavior:

```bash
RUST_LOG=debug cargo run -- --library ./sample-books
```

## Benchmarks

`scripts/bench.sh` measures the v2 performance targets (ingest, feed p95, audio
TTFB, idle RSS) against a synthesized book on your hardware. See
[docs/benchmarks.md](benchmarks.md) for methodology, knobs, and a reference run.

## Release build (static musl)

Release binaries are static musl builds so they run without a glibc dependency (and
so the Docker image can be a tiny runtime-only layer). The bundled SQLite is C, so
the target needs a working C cross-toolchain.

- **Preferred (both arches, uniform):** [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild),
  which uses `zig` as the cross linker.

  ```bash
  rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
  cargo zigbuild --release --target x86_64-unknown-linux-musl --bin podspine
  cargo zigbuild --release --target aarch64-unknown-linux-musl --bin podspine
  ```

- **amd64 alternative:** `musl-tools` (provides `musl-gcc`) with the C compiler
  pointed at it for the bundled SQLite:

  ```bash
  CC_x86_64_unknown_linux_musl=musl-gcc \
    cargo build --release --target x86_64-unknown-linux-musl
  ```

The Docker image is runtime-only: it `COPY`s a prebuilt `dist/<arch>/podspine` into
an Alpine base with `ffmpeg`, so `docker buildx` stays fast across
`linux/amd64,linux/arm64` (arch selected via `TARGETARCH`). The release workflow
produces the per-arch binaries and lays them out under `dist/` — see
[DEPLOYMENT.md](DEPLOYMENT.md) and `.github/workflows/release.yml`.

## Hooks & CI

- **Pre-commit** (`.pre-commit-config.yaml`) runs `cargo fmt --check`, `clippy -D
  warnings`, the lib unit tests, and the changeset lint before a commit lands, plus
  hygiene checks (trailing whitespace, large files, secret/private-key detection).
  A **pre-push** stage additionally runs the full workspace suite plus `cargo audit`
  and `cargo deny` — both binaries must be on `PATH` or the push hook fails. Install
  with `pre-commit install --install-hooks -t pre-commit -t pre-push`.
- **CI** (`.github/workflows/ci.yml`), on PRs and pushes to `main`: `lint`
  (fmt · clippy), a `test` matrix (ubuntu + macos, `cargo nextest` emitting JUnit
  that uploads to Codecov Test Analytics), an informational non-blocking Windows
  leg, `coverage` (cargo-llvm-cov with a **90% line floor** + Codecov upload),
  `supply-chain` (`cargo audit` + `cargo deny`), and a required-checks aggregator.
  Releases run separately on `v*` tags.

Please make sure `fmt`, `clippy` (with `--workspace --all-features`), and
`test --workspace` all pass before opening a PR — CI enforces the same gates.
