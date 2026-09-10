---
default: minor
---

Show the running Podspine version in a footer on every web page, including the first-run scanning page. An operator can now tell which build is deployed without inspecting the container image tag. The value is the compiled workspace version, so a `:nightly` image reports the same version as the release it branches from.
