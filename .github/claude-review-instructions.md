# Claude review instructions

Project rules for the on-demand Claude reviewer (`.github/workflows/claude-review.yml`,
which calls the central job in `schubydoo/shared-workflows`).

This file is read from the base branch, never from the pull request under review. A pull
request therefore cannot edit the rules that govern its own review. Keep it that way. Do
not make the workflow read these instructions from the pull request head.

Tune what the reviewer looks for by editing this file in a normal pull request. How the
review is posted is central: the shared workflow carries that recipe, and it wins on
posting.

Length has a cost. Rules that change review behavior belong here. General project context
belongs in `AGENTS.md` and `CLAUDE.md`, which the reviewer already reads.

---

## Severity

- 🔴 Important. Would break behavior, corrupt a feed, leak data, or violate a safety
  invariant below. Fix before merge.
- 🟡 Nit. Real but minor. Worth saying, never blocking.
- 🟣 Pre-existing. A genuine bug that this pull request did not introduce. Report at most
  two per review and never as Important. This project fixes those in their own pull request.

Style, naming, and refactoring suggestions are Nit at most, always.

## Always check

These are the project's reason-for-existing constraints (`AGENTS.md`). A change that breaks
one is wrong even if the tests pass. Flag it as Important:

1. Sequential pubDates. Feed episode `pubDate` values increase with chapter order, so the
   oldest date is chapter 1. A reversed or equal order makes a podcatcher play episodes out
   of order, which is the project's number-one bug.
2. Real enclosure length. The RSS `enclosure length` is the output file's real byte size
   (`fs::metadata().len()`), never a value prorated from bitrate and duration.
3. Required iTunes tags. Every episode emits `itunes:episode`, `itunes:duration`, and the
   `enclosure length`.
4. Stable guids. `guid = blake3(book.id : idx : source_mtime)`, so a rescan keeps episode
   identity stable.
5. Safe ffmpeg invocation. ffmpeg splits pass `-ss <start>` before `-i` and `-t <duration>`,
   never `-to` after `-i` (which doubles the output). ffmpeg arguments are an argv vector,
   never a shell string, because chapter titles are untrusted input.
6. Path containment. Book and chapter ids are opaque index keys. The resolver canonicalizes
   the path and asserts it stays under the library root, and it returns 404 on reject. A
   `format!("{root}/{user_input}")` path join is a finding.
7. No DRM circumvention. The project never reads Audible AAX/AAXC/.aa, OverDrive, or
   WMA-DRM input. ffmpeg stays out of process (the GPL boundary).
8. Docs in the same pull request. A behavior change updates the affected `docs/` page in the
   same pull request. A new operator option updates the `docs/DEPLOYMENT.md` config table.
9. Comment register. Code comments follow ASD-STE100 Simplified Technical English and pass
   the comment lint (typos, `doc_markdown`, rustdoc). A deliberate test typo must be
   spell-check-safe.

## Do not report

CI already enforces these, and paying a reviewer to re-find them is waste:

- Formatting and lint. `cargo fmt` and `cargo clippy -- -D warnings`.
- Spelling and doc lints. `typos`, `doc_markdown`, and rustdoc.
- Missing coverage as a bare observation. The Codecov patch-coverage gate reports it
  precisely.
- Known-CVE dependencies. `cargo audit`, `cargo deny`, and Renovate.
- Generic OWASP checklist items with no call site in the diff. CodeQL and Scorecard.

Also do not report anything in a `CHANGELOG.md` entry, a generated file, a lockfile, or an
issue silenced in the code by a lint-ignore comment. Do not flag a missing `.changeset/`
fragment. The changeset-check workflow posts that advisory already.

## Review independently

You are a second opinion. Greptile reviews this repository routinely, and Codecov reports
patch coverage. You are asked precisely when an independent read is wanted.

- Do not read other reviewers' comments on the pull request before forming your findings.
  Not Greptile's, not Codecov's. Work from the diff and the code.
- A finding is not more credible because another tool raised it, nor less because it did
  not. Confirming someone else's list is not the job.
- The one exception is your own previous review on the same pull request. Read that one and
  reconcile against it, per the re-review rules below.

## Verification bar

Every finding must be checkable from the code, not inferred from a name.

- A claim about behavior needs a `file:line` citation of the code that causes it.
- If confirming a finding needs context outside the diff, read that context first. If you
  still cannot confirm it, do not post it.
- Do not flag anything whose failure depends on inputs or state you have not shown to be
  reachable.

A false positive costs the author a round trip and costs the reviewer its credibility. When
uncertain, say nothing.

### Do not run the build or the test suite

Reviewing is a reading job here. Do not run `cargo build`, `cargo test`, `cargo clippy`, or
the fuzz targets. A cold build on the runner costs minutes of quota to reproduce what CI
already runs on every pull request for free.

CI is the measurement. When a pull request asserts a test result or a performance number:

- Check that the change could produce it. Read the code, the fixtures, the test.
- Say what you verified and how, and name CI as the gate for the rest. "Verified by reading.
  The CI suite is the measurement" is a complete answer, not an apology.
- Do not frame the absence of a local run as a limitation of the review. It is the design.

Attempting a build anyway is worse than useless. The calls are denied, and every denial is
counted and reported by the workflow's guard step. Routine denials bury the ones that matter.

## Volume

At most five Nits per review. If there are more, post the five that matter and add "plus N
similar nits" to the summary. There is no cap on Important findings.

## Re-reviews

When the pull request has been reviewed before, put a `## Previous findings` section
directly after the tally line and resolve every prior Important finding as exactly one of:

- FIXED. Cite the line or commit that addressed it.
- ACCEPTED. Quote the author's technical justification and say why it resolves the concern.
  "Please approve", "let's proceed", or "this is fine" is not a technical justification.
- STILL OPEN. Not addressed by code or explanation.

A finding marked FIXED or ACCEPTED is closed. Do not re-raise it. After the first review,
post Important findings only, and suppress new Nits entirely, so a one-line fix cannot reach
round seven on style.

## Output

The central workflow writes a posting recipe next to this file, and it governs how the review
is posted: the grouped review, `review.json`, thread replies, the tally line and the allowed
commands. Do not add posting rules here.

Link code in this repo with the full SHA and a line range:
`https://github.com/schubydoo/podspine/blob/<full-sha>/crates/feed/src/lib.rs#L40-L46`
