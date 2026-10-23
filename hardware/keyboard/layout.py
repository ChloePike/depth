# Regenerate the legend sheet: python3 hardware/keyboard/layout.py hardware/keyboard/layout.svg
# Depth keyboard: standard ANSI full size (104), Depth legends on the function row, the system and nav clusters,
# Esc = CANCEL, Enter = GO (Bloomberg-style). 1u = 19.05 mm, drawn 1 mm = 1 unit.
import sys
U = 19.05
COL = {"nav": "#f2c230", "buy": "#22a06b", "sell": "#e5484d", "neu": "#d9d9d9", "std": "#2b2b2b"}
INK = {"nav": "#111", "buy": "#fff", "sell": "#fff", "neu": "#111", "std": "#cfcfcf"}
K = []  # (x, y, w, top, sub, group, h)
def row(y, items, x=0.0):
    for it in items:
        if isinstance(it, (int, float)): x += it; continue
        t, w, *rest = it
        sub, g = (rest + ["", "std"])[:2] if rest else ("", "std")
        K.append((x, y, w, t, sub, g, 1)); x += w
# function row: red CANCEL isolated; green HELP; yellow market keys (complete a typed coin, Bloomberg
# sector-key grammar); white interval keys (view); green BACK; red kill keys isolated top right
row(0, [("CANCEL", 1, "ESC", "sell"), 1,
        ("HELP", 1, "F1", "buy"), ("SPOT", 1, "F2", "nav"), ("MRGN", 1, "F3", "nav"), ("PERP", 1, "F4", "nav"), 0.5,
        ("OPT", 1, "F5", "nav"), ("1m", 1, "F6", "neu"), ("5m", 1, "F7", "neu"), ("15m", 1, "F8", "neu"), 0.5,
        ("1H", 1, "F9", "neu"), ("4H", 1, "F10", "neu"), ("1D", 1, "F11", "neu"), ("BACK", 1, "F12", "buy"), 0.25,
        ("OPEN", 1, "CLOSE", "neu"), ("CXL", 1, "ALL", "sell"), ("FLAT", 1, "MKT", "sell")])
y = 1.25
row(y, [(c, 1) for c in "`1234567890-="] + [("⌫", 2), 0.25, ("BUY", 1, "BID", "buy"), ("BUY", 1, "MKT", "buy"), ("PG", 1, "BACK", "buy")])
row(y + 1, [("Tab", 1.5)] + [(c, 1) for c in "QWERTYUIOP[]"] + [("\\", 1.5), 0.25, ("SELL", 1, "ASK", "sell"), ("SELL", 1, "MKT", "sell"), ("PG", 1, "FWD", "buy")])
row(y + 2, [("Caps", 1.75)] + [(c, 1) for c in "ASDFGHJKL;'"] + [("GO", 2.25, "ENTER", "buy")])
row(y + 3, [("Shift", 2.25)] + [(c, 1) for c in "ZXCVBNM,."] + [("CMD", 1, "/", "neu"), ("Shift", 2.75), 1.25, ("↑", 1)])
row(y + 4, [("Ctrl", 1.25), ("Opt", 1.25), ("Cmd", 1.25), ("", 6.25), ("Cmd", 1.25), ("Opt", 1.25), ("Fn", 1.25), ("Ctrl", 1.25), 0.25, ("←", 1), ("↓", 1), ("→", 1)])
NX = 18.5
for (x, yy, t, h) in [(0, 1.25, "Num", 1), (1, 1.25, "/", 1), (2, 1.25, "*", 1), (3, 1.25, "-", 1),
                      (0, 2.25, "7", 1), (1, 2.25, "8", 1), (2, 2.25, "9", 1), (3, 2.25, "+", 2),
                      (0, 3.25, "4", 1), (1, 3.25, "5", 1), (2, 3.25, "6", 1),
                      (0, 4.25, "1", 1), (1, 4.25, "2", 1), (2, 4.25, "3", 1), (3, 4.25, "Enter", 2),
                      (0, 5.25, "0", 1), (2, 5.25, ".", 1)]:
    K.append((NX + x, yy, 2 if t == "0" else 1, t, "", "std", h))
pad = 8
Wu = max(k[0] + k[2] for k in K); Hu = max(k[1] + k[6] for k in K)
W, H = Wu * U + 2 * pad, Hu * U + 2 * pad
o = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W:.1f} {H + 30:.1f}" width="{W*5:.0f}" height="{(H + 30)*5:.0f}" font-family="Inter, \'Helvetica Neue\', Arial, sans-serif">',
     f'<rect width="{W:.1f}" height="{H + 30:.1f}" fill="#fff"/>',
     f'<rect width="{W:.1f}" height="{H:.1f}" rx="5" fill="#141414"/>']
for x, y, w, t, sub, g, h in K:
    kx, ky, kw, kh = pad + x * U + 0.6, pad + y * U + 0.6, w * U - 1.2, h * U - 1.2
    o.append(f'<rect x="{kx:.2f}" y="{ky:.2f}" width="{kw:.2f}" height="{kh:.2f}" rx="1.6" fill="{COL[g]}"/>')
    o.append(f'<rect x="{kx+1.6:.2f}" y="{ky+1:.2f}" width="{kw-3.2:.2f}" height="{kh-3.6:.2f}" rx="1.2" fill="#fff" fill-opacity="{0.10 if g != "std" else 0.04}"/>')
    cx = kx + kw / 2
    if g == "std":
        o.append(f'<text x="{kx+3:.2f}" y="{ky+6:.2f}" font-size="{3.4 if len(t) == 1 else 2.6}" fill="{INK[g]}">{t}</text>')
        continue
    big = 4.4 if len(t) <= 3 else (3.6 if len(t) <= 4 else 3.0)
    if t in ("◀", "▶"): big = 4.8
    if t == "GO": big = 6.0
    if sub:
        o.append(f'<text x="{cx:.2f}" y="{ky+kh/2:.2f}" text-anchor="middle" font-size="{big}" font-weight="700" fill="{INK[g]}">{t}</text>')
        o.append(f'<text x="{cx:.2f}" y="{ky+kh/2+4.6:.2f}" text-anchor="middle" font-size="2.4" font-weight="600" fill="{INK[g]}" fill-opacity="0.72">{sub}</text>')
    else:
        o.append(f'<text x="{cx:.2f}" y="{ky+kh/2+1.5:.2f}" text-anchor="middle" font-size="{big}" font-weight="700" fill="{INK[g]}">{t}</text>')
# the three indicator lights over the numpad, driven by Depth (not by the lock keys)
for i, (name, col, desc) in enumerate([("LIVE", "#22a06b", "connected, data fresh"), ("POS", "#f2c230", "position held · blinks in loss"), ("ORD", "#4c9eeb", "orders resting · fast blink: confirm / rejected")]):
    lx = pad + (NX + 0.5 + i * 1.25) * U
    o.append(f'<circle cx="{lx:.2f}" cy="{pad + 0.35 * U:.2f}" r="1.6" fill="{col}"/><circle cx="{lx:.2f}" cy="{pad + 0.35 * U:.2f}" r="3.2" fill="{col}" fill-opacity="0.25"/>')
    o.append(f'<text x="{lx:.2f}" y="{pad + 0.35 * U + 7.5:.2f}" text-anchor="middle" font-size="2.6" font-weight="600" fill="#bbb">{name}</text>')
n = sum(1 for k in K if k[5] != "std")
o.append(f'<text x="{pad}" y="{H+11:.1f}" font-size="4.4" font-weight="700" fill="#111">Depth keyboard</text>')
o.append(f'<text x="{pad+36}" y="{H+11:.1f}" font-size="3" fill="#555">standard ANSI 104 · {n} Depth keys · 3 status lights (LIVE · POS · ORD) · type a coin, then a yellow key or GO · Fn restores F1–F12 and the nav cluster</text>')
x = pad
for g, t in [("nav", "market (completes a typed coin)"), ("buy", "go · help · back · page · buy"), ("sell", "cancel · sell · cancel all · flatten"), ("neu", "command · interval · open-close")]:
    o.append(f'<rect x="{x:.1f}" y="{H+17:.1f}" width="5" height="5" rx="1" fill="{COL[g]}"/><text x="{x+7:.1f}" y="{H+21:.1f}" font-size="3" fill="#333">{t}</text>')
    x += 7 + len(t) * 1.45 + 10
o.append('</svg>')
open(sys.argv[1], "w").write("\n".join(o))
print(n, f"{W:.0f} x {H:.0f} mm")
