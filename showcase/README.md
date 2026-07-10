# Showcase assets (untracked — not part of the product repo)

Blog material for the iris TUI redesign, generated 2026-07-09.

- `iris-hardware-hero.png` — the IRIS-1 as physical hardware (Higgsfield Soul,
  1080p). World-building hero: the imaginary bench instrument this TUI belongs to.
- `iris-hardware-alt-gold.png` — earlier candidate (gold faceplate), kept for comparison.
- `boot-frame.ansi` / `settled.ansi` — raw ANSI captures (84×26) of the power-on
  lamp test's all-lit hold frame and the settled start page. Render with `cat`.
- `settings-faceplate.ansi` — the `/settings` control surface (100×42): one flat
  silkscreened panel, every setting adjusted in place — switches as printed
  detent tracks, numerics as 10-LED dials, `▸` ports into deeper surfaces.
  Spec: `docs/TUI_DESIGN_LANGUAGE.md` §10.1.

To record the animated hero cast: `scripts/record-demo.sh` (needs asciinema).
Boot sequence lives on the start page: launch `iris` with no args in a fresh
terminal. `IRIS_REDUCED_MOTION=1` skips all motion.
