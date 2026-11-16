# Decisions

Architecture and product decisions for corvx, with dates and reasons. Changed through proposals accepted in CORVX.

## Website: static, dependency-free, in site/

2026-11-16: the project website lives in `site/` as plain HTML and CSS: no JS, no build step, no trackers, system fonts, relative paths, so it works as a file and on GitHub Pages under /corvx/. `site/assets/demo.gif` is a copy of `docs/demo.gif`; re-copy it when the demo changes. Only claim what the README says, including the "not ready for general use" status.

_Accepted 2026-11-16. Evidence: Website mission (s1794852249-3), branch corvx/s1794852249-3; the requirements came from the human._
