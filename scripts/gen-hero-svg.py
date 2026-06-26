#!/usr/bin/env python3
"""Generate the Iris README hero: an animated terminal-transcript SVG.

Faithful reproduction of the Iris TUI pane grammar (see docs/TUI_DESIGN_LANGUAGE.md):
square box-drawing chrome, the state symbol vocabulary, the LED-chase working
indicator, and the composer statusline. Motion is confined to the LED chase and
the context-meter edge-dot pulse, exactly as the spec allows. Theme-adaptive:
two files, switched by <picture> prefers-color-scheme in the README.
"""
import html, sys

CELL_W = 9.1      # px per cell — kept just under a true monospace advance so
                  # textLength always *compresses* runs into solid box-rules,
                  # never stretches them into a dashed look, in any viewer font.
LINE_H = 22.0     # px per row
FONT_PX = 16
PAD = 26          # px inner padding around the grid
PANEL_W = 60      # inner panel width (between the │ │)
INDENT = 2        # pane indent in cells

# ---- palettes ---------------------------------------------------------------
DARK = dict(
    bg="#1a1a1f", ink="#e6e6e6", border="#73737b", muted="#8b8b93",
    accent="#d78700", success="#5faf5f", danger="#d77272", inter="#3bb5b5",
    addbg="#143a1c", delbg="#3c1616", frame="#3a3a42",
)
LIGHT = dict(
    bg="#f4f4f5", ink="#1d1d21", border="#9a9aa2", muted="#5c5c63",
    accent="#b25f00", success="#2f7d36", danger="#b23b3b", inter="#0e7a7a",
    addbg="#dcefdc", delbg="#f6dede", frame="#d9d9de",
)

# ---- span model -------------------------------------------------------------
# A line is a list of (text, role). Roles map to CSS classes (palette colors).
def S(text, role="ink"):
    return (text, role)

def pad_to(segs, width):
    used = sum(len(t) for t, _ in segs)
    if used < width:
        segs = segs + [S(" " * (width - used), "none")]
    return segs

def border_row(left, right):
    return [S(left, "border"), S("─" * PANEL_W, "border"), S(right, "border")]

def body(inner):
    return [S("│", "border")] + pad_to(inner, PANEL_W) + [S("│", "border")]

def header(left, right):
    used = sum(len(t) for t, _ in left) + sum(len(t) for t, _ in right)
    gap = PANEL_W - used
    inner = left + [S(" " * gap, "none")] + right
    return [S("│", "border")] + inner + [S("│", "border")]

IND = S("  ", "none")            # pane indent (2)

def normalize(line):
    """Push boundary spaces of every drawn run into non-drawn `none` runs so
    renderers can't trim them; interior spaces stay put. Column math is
    unchanged because `none` runs still advance the cursor."""
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

# ---- the transcript ---------------------------------------------------------
lines = []
bg_rows = {}   # row_index -> (role, start_col, width_cells)  for diff tints

def add(*segs):
    lines.append(list(segs))

def blank():
    lines.append([])

# context line
add(IND, S("someotherguy@dev", "muted"), S(" ", "none"), S("~/iris", "muted"))
blank()

# assistant intent
add(IND, S("›", "muted"), S(" ", "none"),
    S("I'll thread the cancellation token through the tool loop.", "ink"))
blank()

# EXPLORE panel
add(IND, *border_row("┌", "┐"))
add(IND, *header(
    [S(" ", "none"), S("▾", "muted"), S("  ", "none"), S("EXPLORE", "label"),
     S("  ", "none"), S("src/nexus.rs", "meta")],
    [S("◆", "success"), S(" ", "none"), S("DONE", "ink"), S("   ", "none"),
     S("0.2s", "muted"), S(" ", "none")]))
add(IND, *border_row("├", "┤"))
add(IND, *body([S("    ", "none"), S("Read", "ink"), S(" ", "none"), S("src/nexus.rs", "muted")]))
add(IND, *body([S("    ", "none"), S("Grep", "ink"), S(" ", "none"), S("\"CancellationToken\"", "muted")]))
add(IND, *border_row("└", "┘"))
blank()

# EDIT panel
add(IND, *border_row("┌", "┐"))
add(IND, *header(
    [S(" ", "none"), S("▾", "muted"), S("  ", "none"), S("EDIT", "label"),
     S("  ", "none"), S("src/nexus.rs", "meta")],
    [S("◆", "success"), S(" ", "none"), S("DONE", "ink"), S("   ", "none"),
     S("0.4s", "muted"), S(" ", "none")]))
add(IND, *border_row("├", "┤"))
# removed row (del-bg tint)
bg_rows[len(lines)] = ("delbg", INDENT + 1, PANEL_W)
add(IND, *body([S("  ", "none"), S("84", "muted"), S("  ", "none"), S("−", "danger"),
                S("  ", "none"), S("let result = tool.execute(args).await?;", "ink")]))
# added row (add-bg tint)
bg_rows[len(lines)] = ("addbg", INDENT + 1, PANEL_W)
add(IND, *body([S("  ", "none"), S("84", "muted"), S("  ", "none"), S("+", "success"),
                S("  ", "none"), S("let result = tool.execute(args, child).await?;", "ink")]))
add(IND, *border_row("└", "┘"))
blank()

# assistant result (inline code in cyan)
add(IND, S("›", "muted"), S(" ", "none"),
    S("Threaded it through ", "ink"), S("execute", "inter"),
    S("; tests pass. The transcript stays valid on abort.", "ink"))
blank()

# turn divider (full panel width)
TOTAL = INDENT + PANEL_W + 2
div_head = [IND, S("──", "border"), S(" 7.6s ", "muted"), S("┊", "muted"),
            S(" ↑18.2k ↓846 ", "muted")]
used = sum(len(t) for t, _ in div_head)
div_head.append(S("─" * (TOTAL - used), "border"))
add(*div_head)
blank()

# working indicator — animated LED chase
WI_ROW = len(lines)
add(IND, S("····", "led-base"), S("  ", "none"), S("┊", "muted"), S(" ESC ", "muted"),
    S("┊", "muted"), S(" ↑177k ↓5.7k", "muted"))
blank()

# composer — statusline top frame
status_left = [S("┌", "border"), S("─ ", "border"), S("◉", "accent"), S(" CODE ", "ink"),
               S("─ ", "border"), S("GPT-5.5 XHIGH ", "ink"), S("─ ", "border"),
               S("CTX 300K ", "muted")]
meter = [S("●", "meter"), S("●", "meter"), S("●", "edge"),
         S("○", "meter"), S("○", "meter"), S("○", "meter"), S("○", "meter"),
         S("○", "meter"), S("○", "meter"), S("○", "meter")]
status = [IND] + status_left + meter + [S(" ", "none")]
content_w = PANEL_W + 2  # 62 cells after the pane indent (┌ + 60 + ┐)
used = sum(len(t) for t, _ in status) - len(IND[0])
status.append(S("─" * (content_w - used - 1), "border"))
status.append(S("┐", "border"))
COMP_TOP = len(lines)
add(*status)
add(IND, *body([S("", "none")]))
add(IND, *body([S("  ", "none"), S("Give Iris a task...", "muted")]))
add(IND, *body([S("", "none")]))
add(IND, *body([S("  ", "none"), S("↵ to send", "muted"), S("  •  ", "muted"),
                S("shift+↵ for new line", "muted"), S("  •  ", "muted"),
                S("/ for commands", "muted")]))
add(IND, *border_row("└", "┘"))
add(S("   ", "none"), S("~/iris", "muted"), S(" ", "none"), S("┊", "muted"),
    S(" ", "none"), S("git main", "muted"))

# ---- emit -------------------------------------------------------------------
def plain():
    out = []
    for ln in lines:
        out.append("".join(t for t, _ in ln))
    return "\n".join(out)

def grid_cols():
    return max((sum(len(t) for t, _ in ln) for ln in lines), default=0)

def svg(pal):
    cols = grid_cols()
    W = cols * CELL_W + 2 * PAD
    H = len(lines) * LINE_H + 2 * PAD
    parts = []
    parts.append(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W:.0f}" height="{H:.0f}" '
        f'viewBox="0 0 {W:.0f} {H:.0f}" role="img" '
        f'aria-label="Iris terminal session: an EXPLORE and EDIT tool turn, a turn divider, the LED working indicator, and the composer.">')
    # styles
    css = f"""
  text {{ font-family: 'DejaVu Sans Mono','Liberation Mono','Cascadia Mono','JetBrains Mono',Menlo,Consolas,'Courier New',monospace; font-size:{FONT_PX}px; white-space:pre; dominant-baseline:middle; }}
  .ink{{fill:{pal['ink']};}} .muted{{fill:{pal['muted']};}} .border{{fill:{pal['border']};}}
  .label{{fill:{pal['ink']};font-weight:700;}} .meta{{fill:{pal['muted']};}}
  .accent{{fill:{pal['accent']};}} .success{{fill:{pal['success']};}} .danger{{fill:{pal['danger']};}}
  .inter{{fill:{pal['inter']};}} .meter{{fill:{pal['muted']};}}
  .led-base{{fill:{pal['muted']};}}
  .led{{fill:{pal['accent']};opacity:0;}}
  .edge{{fill:{pal['accent']};}}
  #l0{{opacity:1;animation:chase 1.6s steps(1) infinite;}}
  #l1{{animation:chase 1.6s steps(1) infinite;animation-delay:0.4s;}}
  #l2{{animation:chase 1.6s steps(1) infinite;animation-delay:0.8s;}}
  #l3{{animation:chase 1.6s steps(1) infinite;animation-delay:1.2s;}}
  @keyframes chase{{0%,25%{{opacity:1;}}25.01%,100%{{opacity:0;}}}}
  #edgedot{{animation:pulse 2.4s ease-in-out infinite;}}
  @keyframes pulse{{0%,100%{{opacity:0.55;}}50%{{opacity:1;}}}}
  @media (prefers-reduced-motion: reduce){{
    #l0,#l1,#l2,#l3{{animation:none;}} #l0{{opacity:1;}} #edgedot{{animation:none;opacity:1;}}
  }}
"""
    parts.append(f"<style>{css}</style>")
    # background + frame (square corners)
    parts.append(f'<rect x="0" y="0" width="{W:.0f}" height="{H:.0f}" fill="{pal["bg"]}"/>')
    parts.append(f'<rect x="0.5" y="0.5" width="{W-1:.0f}" height="{H-1:.0f}" fill="none" stroke="{pal["frame"]}" stroke-width="1"/>')
    # diff bg tints
    for row, (role, start, width) in bg_rows.items():
        x = PAD + start * CELL_W
        y = PAD + row * LINE_H
        parts.append(f'<rect x="{x:.1f}" y="{y:.1f}" width="{width*CELL_W:.1f}" height="{LINE_H:.1f}" fill="{pal[role]}"/>')
    # text spans
    edge_idx = 0
    led_idx = 0
    for r, ln in enumerate(lines):
        col = 0
        ybase = PAD + r * LINE_H + LINE_H / 2 + 1
        for text, role in normalize(ln):
            n = len(text)
            if text.strip() == "" and role in ("none", "muted", "border") and role != "border":
                col += n
                continue
            if role == "none":
                col += n
                continue
            x = PAD + col * CELL_W
            tl = n * CELL_W
            esc = html.escape(text, quote=True)
            if role == "led-base":
                # four base dots + four animated overlay dots
                for i, ch in enumerate(text):
                    cx = PAD + (col + i) * CELL_W
                    parts.append(f'<text class="led-base" x="{cx:.1f}" y="{ybase:.1f}" textLength="{CELL_W:.1f}" lengthAdjust="spacingAndGlyphs">·</text>')
                    parts.append(f'<text id="l{i}" class="led" x="{cx:.1f}" y="{ybase:.1f}" textLength="{CELL_W:.1f}" lengthAdjust="spacingAndGlyphs">●</text>')
                col += n
                continue
            cls = role
            extra = ""
            if role == "edge":
                cls = "edge"
                extra = ' id="edgedot"'
            parts.append(
                f'<text{extra} class="{cls}" x="{x:.1f}" y="{ybase:.1f}" '
                f'textLength="{tl:.1f}" lengthAdjust="spacingAndGlyphs">{esc}</text>')
            col += n
    parts.append("</svg>")
    return "\n".join(parts)

if __name__ == "__main__":
    base = sys.argv[1] if len(sys.argv) > 1 else "."
    with open(f"{base}/hero-dark.svg", "w") as f:
        f.write(svg(DARK))
    with open(f"{base}/hero-light.svg", "w") as f:
        f.write(svg(LIGHT))
    print(plain())
    print("\n--- grid cols:", grid_cols(), "lines:", len(lines))
