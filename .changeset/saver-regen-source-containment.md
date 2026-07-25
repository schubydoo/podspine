---
default: patch
---

Harden the saver-mode chapter-regeneration path: the book source is now canonicalized and asserted to live under the library root before it can reach ffmpeg, matching the checks the serve-in-place and faststart-remux paths already had
