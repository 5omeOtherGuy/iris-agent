#!/usr/bin/env python3
"""Generate the Iris README banner: a minimal animated terminal micro-transcript.

A three-beat exchange in the Iris pane grammar (see docs/TUI_DESIGN_LANGUAGE.md):
a muted user question, the LED-chase working indicator (the one live element),
then the assistant answer — the tagline. Conversation is never boxed, so the
banner is pure transcript: short, calm, instrument-like. Theme-adaptive: two
files switched by <picture> prefers-color-scheme in the README.

    python3 scripts/gen-hero-svg.py docs/assets
"""
import html, sys

CELL_W = 9.1      # px per cell — just under a true monospace advance so
                  # textLength compresses runs into solid rules, never gaps.
LINE_H = 22.0
FONT_PX = 16
PAD = 24          # px inner padding
MIN_COLS = 64     # canvas floor so the strip reads as a banner, not a tight box

DARK = dict(bg="#1a1a1f", ink="#e6e6e6", muted="#8b8b93", accent="#d78700",
            frame="#33333b")
LIGHT = dict(bg="#f4f4f5", ink="#1d1d21", muted="#5c5c63", accent="#b25f00",
             frame="#dcdce1")

TAGLINE = "A precise, token-efficient coding agent for the terminal."

def S(text, role="ink"):
    return (text, role)

def normalize(line):
    """Push boundary spaces of drawn runs into non-drawn `none` runs so
    renderers can't trim them; column math is unchanged."""
    out = []
    for text, role in line:
        if role == "none" or text.strip() == "":
            out.append((text, "none")); continue
        lead = len(text) - len(text.lstrip(" "))
        trail = len(text) - len(text.rstrip(" "))
        core = text[lead:len(text) - trail]
        if lead: out.append((" " * lead, "none"))
        out.append((core, role))
        if trail: out.append((" " * trail, "none"))
    return out

# ---- the banner -------------------------------------------------------------
# Columns: marker at 2, transcript text at 4 (matches the live pane grid).
lines = [
    [S("    ", "none"), S("What are you?", "muted")],
    [],
    [S("  ", "none"), S("····", "led-base"), S("  ", "none"), S("0.6s", "muted")],
    [],
    [S("  ", "none"), S("›", "muted"), S(" ", "none"), S(TAGLINE, "ink")],
]

def grid_cols():
    return max((sum(len(t) for t, _ in ln) for ln in lines), default=0)

def svg(pal):
    cols = max(grid_cols(), MIN_COLS)
    W = cols * CELL_W + 2 * PAD
    H = len(lines) * LINE_H + 2 * PAD
    p = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W:.0f}" height="{H:.0f}" '
         f'viewBox="0 0 {W:.0f} {H:.0f}" role="img" '
         f'aria-label="What are you? — {html.escape(TAGLINE)}">']
    css = f"""
  text {{ font-family: 'DejaVu Sans Mono','Liberation Mono','Cascadia Mono','JetBrains Mono',Menlo,Consolas,'Courier New',monospace; font-size:{FONT_PX}px; white-space:pre; dominant-baseline:middle; }}
  .ink{{fill:{pal['ink']};}} .muted{{fill:{pal['muted']};}}
  .led-base{{fill:{pal['muted']};}}
  .led{{fill:{pal['accent']};opacity:0;}}
  #l0{{opacity:1;animation:chase 1.6s steps(1) infinite;}}
  #l1{{animation:chase 1.6s steps(1) infinite;animation-delay:0.4s;}}
  #l2{{animation:chase 1.6s steps(1) infinite;animation-delay:0.8s;}}
  #l3{{animation:chase 1.6s steps(1) infinite;animation-delay:1.2s;}}
  @keyframes chase{{0%,25%{{opacity:1;}}25.01%,100%{{opacity:0;}}}}
  @media (prefers-reduced-motion: reduce){{
    #l0,#l1,#l2,#l3{{animation:none;}} #l0{{opacity:1;}}
  }}
"""
    p.append(f"<style>{css}</style>")
    p.append(f'<rect width="{W:.0f}" height="{H:.0f}" fill="{pal["bg"]}"/>')
    p.append(f'<rect x="0.5" y="0.5" width="{W-1:.0f}" height="{H-1:.0f}" fill="none" stroke="{pal["frame"]}" stroke-width="1"/>')
    for r, ln in enumerate(lines):
        col = 0
        y = PAD + r * LINE_H + LINE_H / 2 + 1
        for text, role in normalize(ln):
            n = len(text)
            if role == "none":
                col += n; continue
            x = PAD + col * CELL_W
            if role == "led-base":
                for i in range(n):
                    cx = PAD + (col + i) * CELL_W
                    p.append(f'<text class="led-base" x="{cx:.1f}" y="{y:.1f}" textLength="{CELL_W:.1f}" lengthAdjust="spacingAndGlyphs">·</text>')
                    p.append(f'<text id="l{i}" class="led" x="{cx:.1f}" y="{y:.1f}" textLength="{CELL_W:.1f}" lengthAdjust="spacingAndGlyphs">●</text>')
                col += n; continue
            p.append(f'<text class="{role}" x="{x:.1f}" y="{y:.1f}" textLength="{n*CELL_W:.1f}" '
                     f'lengthAdjust="spacingAndGlyphs">{html.escape(text)}</text>')
            col += n
    p.append("</svg>")
    return "\n".join(p)

if __name__ == "__main__":
    base = sys.argv[1] if len(sys.argv) > 1 else "."
    open(f"{base}/hero-dark.svg", "w").write(svg(DARK))
    open(f"{base}/hero-light.svg", "w").write(svg(LIGHT))
    print("\n".join("".join(t for t, _ in ln) for ln in lines))
    print("\n--- cols:", max(grid_cols(), MIN_COLS), "lines:", len(lines))
