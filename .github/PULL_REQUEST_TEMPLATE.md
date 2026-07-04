## Description

What does this change and why?

## Related issue

Fixes #(issue number)

## Type of change

- [ ] Bug fix (non-breaking change that fixes an issue)
- [ ] New feature (non-breaking change that adds functionality)
- [ ] Breaking change (would change existing behavior)
- [ ] Documentation
- [ ] Refactor / internal (no behavior change)

## Checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes (with `ffmpeg`/`ffprobe` on PATH)
- [ ] Added or updated tests for the change
- [ ] Updated docs (`README.md` / `docs/`) and `CHANGELOG.md` if user-visible
- [ ] Commits follow Conventional Commits; PR targets `main`
- [ ] Kept Podspine's invariants (argv-vector ffmpeg calls, opaque ids under the
      data dir, sequential feed `pubDate`s, DRM-free-only)

## Notes for reviewers

Anything that needs extra attention, or manual steps to verify.
