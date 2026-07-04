# Changesets

Podspine's releases are **changesets-only**: the version bump and the CHANGELOG
are driven by the `.changeset/*.md` fragments in this directory (via
[knope](https://knope.tech/), configured in `knope.toml`), **not** by commit
messages. A user-facing change with no fragment ships with no changelog entry and
no version bump.

## Add one to your PR

Either run `knope document-change` (it scaffolds a fragment interactively), or add
a file `.changeset/<short-slug>.md`:

```markdown
---
default: minor
---

Ingest Opus files with embedded chapters

An optional second paragraph becomes the entry's details in the changelog.
```

- **Front-matter** maps the change to a bump/section. Use `default: <type>` where
  `<type>` is one of:
  - `major` — a breaking change
  - `minor` — a new feature (→ **Features**)
  - `patch` — a bug fix (→ **Fixes**)
  - `security` — a security fix (→ **Security**)
  - `perf` — a performance improvement (→ **Performance**)
- **Body**: the first line is the summary shown in the changelog; the rest is
  optional detail. knope appends the PR link automatically at release time.

## When you don't need one

Internal-only PRs (CI, refactors, tests, docs that aren't user-facing) don't need
a fragment — add the **`no-changelog`** label to silence the advisory
`changeset-check`. The release automation is currently gated off (`KNOPE_ENABLED`);
fragments still accumulate here and are consumed on the first enabled release.
