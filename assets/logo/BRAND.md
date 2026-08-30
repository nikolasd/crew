# crew

*(Greek: πλήρωμα, romanized: plḗrōma)*

**Brand and logo usage guidelines — v1.0**

---

## 01 · The mark

Crew inherits the Oh-My-Pi silhouette — an overhanging bar with two legs set inside it, the right leg longer than the left — and breaks the legs into discrete cells. Each cell is one agent the orchestrator can spawn. The bar is Crew itself: the orchestrator holding the crew together.

**Construction.** Drawn on a 32×32 grid. The bar is 31×5 at (0.5, 0.5). Cells are 5×5 with a 1.5 gutter, in two columns at x = 7 and x = 20; rows at y = 7, 13.5, 20 and 26.5. The left column stops at three cells, the right runs to four. Every coordinate is a whole or half unit, so the mark stays crisp at 16px.

**Never redraw it.** Use the supplied files. Do not rebuild the mark by eye, change the cell count, even the leg lengths, re-space the gutters, or round the corners. The unequal legs are the family tie to Oh-My-Pi, and the square corners are what let it survive as an ASCII glyph.

---

## 02 · Cell colours

One cell, one colour, one runtime — each taken from that tool's own brand. The dimmed slot is deliberately empty: it is the seat waiting for whatever you add next. The bar keeps the Oh-My-Pi gradient and is the only gradient in the system.

| Slot | Runtime | Value |
| --- | --- | --- |
| Bar | Oh-My-Pi / Crew | `#f74fcc` → `#9362f4` |
| L1 | Claude Code | `#d97757` |
| L2 | Hermes | `#d9a441` |
| L3 | Open slot — dimmed | `#524b8e` |
| R1 | opencode | `#fdfcfc` |
| R2 | GitHub Copilot | `#58a6ff` |
| R3 | Codex | `#10a37f` |
| R4 | omp — the deepest leg | `#28bae7` |

Adding a runtime: fill the dimmed L3 slot with its brand colour before extending the grid. If the mark ever needs a ninth cell, add a row to the left column first — the right leg must stay the longest.

---

## 03 · Backgrounds

Crew is a dark-ground mark. The opencode cell is off-white, so on a light surface it vanishes and the mark reads as missing a piece.

**Do.** Place the full-colour mark on `#161826`, on any near-black or dark terminal ground, or use the transparent files over a dark surface. Keep the ground flat — no photographs or gradients behind it.

**Don't.** Never put the full-colour mark on white or a light tint. On light surfaces use `crew-logo-mono.svg`, which fills from `currentColor` and takes the surrounding text colour.

---

## 04 · Clear space and minimum size

Clear space on all four sides is one cell — 5 grid units, or roughly 16% of the mark's width. Nothing enters it: no type, no rules, no other logos, no container edge.

**Minimum sizes**

- **16px** — absolute floor, favicon only, from the supplied 16px PNG rather than a downscaled SVG.
- **24px** — floor for the full-colour mark in UI.
- **32px** — floor when the mark sits beside the wordmark.

**Scaling.** Scale in whole multiples of the 32-unit grid wherever you can — 32, 64, 128, 256, 512 — so cell edges land on whole pixels. Never distort: the mark scales uniformly or not at all.

---

## 05 · Name, gloss and subtitle

The name is **crew**, set lowercase in Inter Medium with −0.03em tracking. Never capitalise it in the wordmark; in running prose, sentence case ("Crew") is fine.

The Greek gloss sets Wikipedia-style directly under the wordmark: *(Greek: πλήρωμα, romanized: plḗrōma)* — labels muted, the Greek a step brighter, the romanization italic. It is optional on small lockups and never appears without the wordmark above it.

The subtitle is **"Orchestrate your agents as agents, not as sub-agents."** Set it once per surface, in Inter Regular at roughly a quarter of the wordmark's size. It is a sentence, not a tagline lockup — do not letterspace or uppercase it.

---

## 06 · Lockups

- **Horizontal** — mark left, wordmark right, gap equal to one cell. The default for headers, READMEs and nav bars.
- **Stacked** — mark above, wordmark centred below at one cell's distance. For square crops and avatars only.
- **Mark alone** — favicons, app icons, terminal glyphs, and anywhere the name is already set nearby.

Do not co-lock Crew with the Oh-My-Pi mark side by side — the shared silhouette makes the pair read as one broken logo. Name omp in the surrounding copy instead.

---

## 07 · In the terminal

The mark was drawn so it survives as text. Each 5×5 cell becomes a 2×1 block of `█`, keeping the silhouette at character resolution.

```
████████████████
    ██    ██
    ██    ██
    ██    ██
          ██
```

Use the `.ans` files where 24-bit colour is available and the plain `.txt` files everywhere else — never emit raw escape codes when `NO_COLOR` is set or output is not a TTY. Keep the banner to one blank line above and below, and never pad it with a box-drawing frame.

---

## 08 · Never

- Recolour a cell to anything but its runtime's own brand colour.
- Apply a gradient, shadow, bevel, glow or outline to the cells — the bar's gradient is the only one in the system.
- Rotate, skew, stretch, or set the mark on an angle.
- Round the cell corners or convert the mark to circles.
- Place the mark inside a badge, pill or coloured tile that is not the brand ground.
- Animate the cells as a loading spinner — the mark is not a progress indicator.
- Rewrite the subtitle. Change it in this document, then change it everywhere.

---

## 09 · Files

Everything ships from the `export/` folder. Regenerate rasters from the master SVG rather than resampling a PNG.

| File | Use |
| --- | --- |
| `crew-logo.svg` | The master. Everything else derives from it. |
| `crew-logo-mono.svg` | One-colour, fills from currentColor. Light grounds, print, embeds. |
| `crew-logo-16…512.png` | Transparent raster at each hinted size. |
| `favicon.ico`, `favicon-16/32` | Browser tab. |
| `apple-touch-icon.png` | iOS home screen, 180px on the brand ground. |
| `banner/crew-banner-*` | README header, 1280×320 and 2×. |
| `banner/crew-social-*` | GitHub social preview, 1280×640 and 2×. |
| `banner/crew-lockup-*` | Compact mark + wordmark, 640×160 and 2×. |
| `banner/transparent/` | The same six with no ground. Dark surfaces only. |
| `terminal/` | ANSI and plain-text banners, plus `crew-logo.sh`. |

---

**Changing any of this.** These rules exist so the mark survives contact with a terminal, a favicon and a README at the same time. If a rule is in the way, change it here first and reissue the version number — do not make a local exception in one surface.
