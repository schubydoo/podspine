# Contributing to Podspine

Thanks for your interest in improving Podspine! This is a small, deliberately
narrow tool — files in, per-chapter podcast feed out — so the best contributions
keep that scope sharp. Bug fixes, format/edge-case handling, and docs are all
very welcome.

## Before you start

- For anything non-trivial, please **open an issue first** so we can agree on the
  approach before you write code.
- Check existing [issues](https://github.com/schubydoo/podspine/issues) and the
  [CHANGELOG](CHANGELOG.md) so you're not duplicating work.
- Out of scope by design: user accounts, a built-in player, ebooks, a full media
  library, and **any form of DRM circumvention** — please don't propose these.

## Development setup

Podspine is a Rust workspace (one crate per pipeline stage). You need:

- A recent stable **Rust** toolchain (see `rust-toolchain.toml` / `Cargo.toml`
  for the minimum version).
- **`ffmpeg` and `ffprobe` on your `PATH`** — the prober/splitter shell out to
  them, and several tests synthesize fixtures with ffmpeg.

```bash
cargo build
cargo run -- --library ./sample-books   # → http://localhost:8080
```

## Checks (all must pass — CI enforces them)

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

Please run these locally before opening a PR. Tests that need ffmpeg are gated on
its availability, so run them with ffmpeg installed to actually exercise them.

## Branching & commits

- Branch off `main`; PRs target `main`.
- Use [Conventional Commits](https://www.conventionalcommits.org/) for messages,
  e.g. `feat(scanner): …`, `fix(feed): …`, `docs: …`.
- Keep changes focused and the diff minimal — small, reviewable PRs merge faster.

## Pull requests

- Fill in the PR template and link the issue it addresses.
- Include tests for new behavior or bug fixes where practical.
- Update the docs (`README.md`, `docs/`) when your change is user-visible.
- **Add a changeset** for any user-visible change: a `.changeset/*.md` fragment
  (run `knope document-change`, or see [`.changeset/README.md`](.changeset/README.md)).
  Releases and the `CHANGELOG.md` are generated from these fragments — don't
  hand-edit the changelog. Internal-only PRs (CI, refactor, tests) can skip it
  with the `no-changelog` label.

## A few project-specific rules worth knowing

These are the invariants that keep feeds correct and the server safe — reviewers
will look for them:

- **Feed ordering is sacred.** `pubDate`s are sequential with oldest = chapter 1;
  always emit `itunes:episode`, `itunes:duration`, and an `enclosure length` read
  from the real output file (never prorated from bitrate).
- **ffmpeg is invoked with an argv vector, never a shell string** — chapter
  titles and filenames are untrusted.
- **Book/episode ids are opaque keys.** Never build a path from user input;
  canonicalize and assert it stays under the data directory.
- Keep `ffmpeg`/`ffprobe` out-of-process (the GPL boundary) and DRM-free-only.

## License

Podspine is licensed under **AGPL-3.0-only**. By contributing, you agree that
your contributions are licensed under the same terms.
