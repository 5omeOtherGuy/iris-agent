# Showcase asset manual

This directory holds tracked visual material for demonstrating Iris's terminal
interface. It is documentation collateral, not runtime input: the binary does not
load these files and tests do not use them as golden snapshots.

## Inventory

| Asset | Purpose | Reproduce or inspect |
| --- | --- | --- |
| `iris-hardware-hero.png` | Primary world-building image: the fictional IRIS-1 bench instrument behind the TUI design language. Generated with Higgsfield Soul at 1080p on 2026-07-09. | Open as an image. |
| `iris-hardware-alt-gold.png` | Earlier gold-faceplate candidate retained for visual comparison. | Open as an image. |
| `boot-frame.ansi` | Raw 84×26 ANSI capture of the power-on lamp test's all-lit hold frame. | Run `cat showcase/boot-frame.ansi` in a capable terminal. |
| `settled.ansi` | Raw 84×26 ANSI capture of the settled start page. | Run `cat showcase/settled.ansi`. |
| `settings-faceplate.ansi` | Raw 100×42 ANSI capture of `/settings`: a flat faceplate with switches, 10-LED numeric dials, and `▸` links into deeper controls. | Run `cat showcase/settings-faceplate.ansi`; compare with §10.1 of `docs/TUI_DESIGN_LANGUAGE.md`. |

These captures document a point in time. Verify current behavior in the running
binary and `src/ui/` before using them as implementation evidence.

## Record the animated terminal demo

The repository script requires `asciinema`:

```bash
scripts/record-demo.sh
```

To observe the boot sequence directly, launch `iris` without arguments in a fresh,
capable terminal. `IRIS_REDUCED_MOTION=1` skips the animated sequence.

## Ownership rules for agents

- Keep source captures and generated marketing renders clearly distinguished.
- Do not make runtime behavior depend on showcase files.
- When replacing a capture, record its terminal dimensions and the UI state that
  produced it.
- Update this inventory when adding, renaming, or removing an asset.
- Use `docs/TUI_DESIGN_LANGUAGE.md` and current UI code as the product source of
  truth; this directory illustrates them but does not define them.
