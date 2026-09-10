---
default: patch
---

Fix a per-book `.podspine.toml` that sets `metrics_bind` failing the whole sidecar. `metrics_bind` is a server-global key. It is now accepted and ignored with a warning, like the other server-global keys, rather than rejected as an unknown field. A book with `metrics_bind` in its sidecar keeps its other overrides instead of losing all of them.
