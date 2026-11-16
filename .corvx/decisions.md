# Decisions

Architecture and product decisions for corvx, with dates and reasons. Changed through proposals accepted in CORVX.

## Website: static, dependency-free, in site/

2026-11-16: the project website lives in `site/` as plain HTML and CSS: no JS, no build step, no trackers, system fonts, relative paths, so it works as a file and on GitHub Pages under /corvx/. `site/assets/demo.gif` is a copy of `docs/demo.gif`; re-copy it when the demo changes. Only claim what the README says, including the "not ready for general use" status.

_Accepted 2026-11-16. Evidence: Website mission (s1794852249-3), branch corvx/s1794852249-3; the requirements came from the human._

## Website look: the TUI's terminal identity

2026-11-16: the site's identity is the TUI's.
- **Theme:** dark only (#0d1117 background), monospace throughout, with 1px BORDER (#262d36) frames like the TUI columns.
- **Colours:** exactly as in `crates/corvx/src/tui.rs`. ACCENT #79c0ff is the main accent, GREEN and RED mark done and failed, and LEAD purple #bc8cff marks the lead and knowledge. AMBER is only for a "needs you" cue, never an accent.
- **Logo:** the ASCII `LOGO` const, copied by script (never retyped), coloured like `logo_spans()` (pixels rgb(220,220,220), ░ shadows rgb(110,110,110)), with line-height 1. It is used wherever the wordmark appears.

When the TUI's colours or logo change, update the site to match.

_Accepted 2026-11-16. Evidence: Human's review of the website (follow-up round, commit c3ed163 on corvx/s1794852249-3): "no yellow/amber", "use the CORVX ASCII logo exactly as in the TUI", "lean into the retro terminal vibe"._
