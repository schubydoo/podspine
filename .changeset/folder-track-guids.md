---
default: patch
---

Give a folder book's episodes an identity that follows the file. A track's guid was built from its position and the folder's newest track timestamp, so deleting one track renumbered every track after it and handed a later track the guid a subscriber already held, and their podcast app kept the audio it had downloaded under that guid. A track's guid is now built from the file's own name and its own timestamp, so adding, deleting, or replacing one track leaves every other guid alone. One cost on the first scan after the upgrade: every folder book's episodes get new guids once, so a podcast app downloads those books again. Chaptered books are unchanged.
