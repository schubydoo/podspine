---
default: patch
---

Let the database hold the one-source-one-feed rule. A book's source path is now unique in the index, so nothing can add a second book, and a second feed URL, for audio that is already indexed. The scan already assigned ids so that this held, and the rule is now a floor under it rather than a promise. A database that already held two rows for one source keeps the older one, which is the feed its subscribers have held longest, and the other is removed when the server next starts.
