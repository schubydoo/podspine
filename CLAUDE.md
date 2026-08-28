# CLAUDE.md

**The shared agent instructions for this repo live in [AGENTS.md](AGENTS.md). The `@`
line below imports it, so it loads with this file — you don't need to open it separately.**

@AGENTS.md

⚠️ **That `@AGENTS.md` line is load-bearing — do not "tidy" it into a plain link.**
Claude Code reads `CLAUDE.md`, not `AGENTS.md`; a markdown link is a suggestion an agent
may or may not follow, while `@` is an import expanded into context at launch. `@` is inert
inside backticks or code fences, so `` `@AGENTS.md` `` in prose does NOT import.

---

`CLAUDE.local.md` is gitignored and loads *after* this file — host-specific paths, the
local planning docs, and personal tooling notes belong there, not here.
