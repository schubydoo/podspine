---
default: minor
---

Add a `--log-level` flag (`PODSPINE_LOG_LEVEL` env, `log_level` TOML key) to set log verbosity without `RUST_LOG`. Accepted values are `error`, `warn`, `info` (default), `debug`, and `trace`. When `RUST_LOG` is set, it still wins and keeps its per-module directives such as `podspine_scanner=debug`. If the level is unrecognized, Podspine uses `info` and logs a warning instead of aborting startup.
