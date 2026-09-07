# Vendor mark sources

Every file in this directory is an unmodified vendor or upstream-project mark: no
recoloring, no reproportioning, no `filter:` treatments. Used only as a row icon
naming which adapter a run belongs to -- never as a standalone badge, never implying
endorsement.

## claude-code.svg

- **Source:** supplied by the maintainer directly.
- **Added:** 2026-09-07
- **Guideline URL:** PENDING -- not yet confirmed; ask before treating the terms
  below as final.
- **License / terms:** trademark/brand guidelines, not an open-source license --
  displaying the unaltered mark to identify the adapter it names is the intended
  permitted use, but the exact guideline text has not been cited here yet.
- **Fixed color:** `#D97757` -- BRAND.md §02's own Claude cell color.

## codex-openai.svg

- **Source:** supplied by the maintainer directly.
- **Added:** 2026-09-07
- **Guideline URL:** PENDING -- not yet confirmed; ask before treating the terms
  below as final.
- **License / terms:** trademark/brand guidelines, not an open-source license --
  same caveat as `claude-code.svg`.
- **Color:** `fill="currentColor"` -- inherits the surrounding text color; the file
  itself carries no fixed color.

## github-copilot.svg

- **Source:** supplied by the maintainer directly.
- **Added:** 2026-09-07
- **Guideline URL:** PENDING -- not yet confirmed; ask before treating the terms
  below as final.
- **License / terms:** trademark/brand guidelines, not an open-source license --
  same caveat as `claude-code.svg`.
- **Fixed color:** `#fff` (white). Non-square: `viewBox="0 0 256 208"` -- render in
  a fixed-height box with width auto, never forced square, or the aspect distorts
  (itself a prohibited alteration).

## omp.svg

- **Source URL:** `https://raw.githubusercontent.com/can1357/oh-my-pi/main/assets/icon.svg`
- **Fetched:** 2026-09-07, via `gh api repos/can1357/oh-my-pi/contents/assets/icon.svg`
- **License:** MIT (can1357/oh-my-pi)
- **Verified:** byte-identical to the file at that URL (`diff` against a fresh
  fetch, zero-length license check via `gh api repos/can1357/oh-my-pi/license`)
- **Fixed color:** `#fafafa`.

## Why no light-theme variants

The dashboard is dark-ground only (`page.rs`'s own comment, BRAND.md §03). Every
mark above is either a fixed light color, a fixed brand color already legible on
dark, or `currentColor` -- all read correctly with no per-mark light variant. If
the dashboard ever grows a light theme, each mark needs its own light-safe variant
from the vendor's own guidelines before that theme can use this row; do not assume
these files work on a light ground.
