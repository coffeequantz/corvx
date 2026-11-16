# Conventions

How things are done in corvx: style, patterns, testing rules, pitfalls. Changed through proposals accepted in CORVX.

## Commits: author coffeequantz, no Co-Authored-By trailer

Commits in this repo are authored by the configured git user (coffeequantz) only, with no "Co-Authored-By" or "Generated with Claude Code" lines and no private details (emails, names) in messages or files. Claude Code agents add the trailer by default, so tell each agent this explicitly and give it a plain `git commit -m "..."` to use. Check with `git log -1 --format='%an%n%B'` before accepting.

_Accepted 2026-11-16. Evidence: Website mission (s1794852249-3): the human's rules for the mission. The agent complied when told explicitly; both commits checked._
