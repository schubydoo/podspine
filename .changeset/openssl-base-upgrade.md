---
default: patch
---

The Docker image now runs `apk upgrade` before installing runtime dependencies, so openssl (libcrypto3/libssl3) and other base packages get the patched release from the Alpine branch instead of the older version frozen in the pinned base image
