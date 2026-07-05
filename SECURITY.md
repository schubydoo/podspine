# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities **privately** through GitHub's
[private vulnerability reporting](https://github.com/schubydoo/podspine/security/advisories/new)
(the **"Report a vulnerability"** button on the repository's **Security** tab). Do
**not** open a public issue for security reports.

Please include, where possible: the type of issue, the affected component/path,
step-by-step reproduction, and a proof-of-concept if you have one. You can expect
an initial response within a few days. Once a fix is ready we'll coordinate
disclosure and credit you, if you'd like.

## Supported versions

Podspine is pre-1.0 and under active development; only the latest release
receives security fixes.

## Scope & threat model

Podspine is **trusted, host-local infrastructure** for a homelab — it turns a
folder of audiobooks into podcast feeds on the machine it runs on. It is not a
multi-tenant service. Key considerations:

- **No built-in login; two exposure surfaces.** Feed/audio/cover routes are
  protected by an unguessable per-book **capability URL** (`feed_id`) and are safe
  to expose to a network. The **browse UI** (`/`, `/book/*`) enumerates the whole
  library and must stay on the LAN or behind a trusted reverse proxy with auth —
  it hands out those capability URLs. The state-changing `POST /book/*/regenerate`
  shares the UI boundary and is additionally same-origin/CSRF-guarded. See
  [DEPLOYMENT.md](docs/DEPLOYMENT.md#exposing-podspine-safely).
- **The library and data directory are trusted inputs.** Podspine reads the
  library you point it at and writes split episodes + a SQLite index into the
  data directory. Point it only at content you control.
- **DRM-free input only.** DRM-protected files (Audible `.aax`/`.aaxc`/`.aa`,
  OverDrive `.odm`) are skipped with a logged notice; Podspine ships **no** DRM
  circumvention.
- **ffmpeg/ffprobe run out of process** (a GPL boundary) and are always invoked
  with an argument vector, never a shell string.

The headline risks we actively guard against, and welcome reports on:

- **Path traversal** in resolving a book/episode id to a file — ids are opaque
  index keys validated against an allow-list and resolved server-side; the
  resolved path is canonicalized and must stay under the data directory (404 on
  reject).
- **Command injection** into the ffmpeg/ffprobe argv from untrusted metadata
  (chapter titles, filenames, tags) — arguments are always passed as a vector.

Reports that require already having shell/host access, or that amount to "the
operator can manage their own host," are generally out of scope.

## Release artifacts

Releases are built in GitHub Actions and signed. Each GitHub Release includes a
`checksums.txt` over the binaries and SBOMs, a keyless **cosign** signature
(`checksums.txt.sigstore.json`), and a **SLSA build-provenance** file
(`podspine.intoto.jsonl`); a CycloneDX SBOM is attached per binary. The GHCR
image carries SLSA provenance plus a cosign signature. Verify before running:

```bash
cosign verify-blob --bundle checksums.txt.sigstore.json checksums.txt
sha256sum -c checksums.txt          # then check your download against it
```

(and `cosign verify` / `gh attestation verify` for the container image).
