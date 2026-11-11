"""Icône de Kite PDF : cerf-volant incliné sur un carré arrondi encre, un pan en couleur d'accent.
Dessinée en vectoriel puis réduite (suréchantillonnage x8) pour chaque taille du .ico ; sous 40 px,
la queue disparaît et le cerf-volant grossit pour rester lisible."""
import math, sys
from PIL import Image, ImageDraw

BG = (17, 24, 39)        # encre
FG = (245, 247, 250)     # blanc cassé
ACCENT = (255, 176, 32)  # ambre

def bez3(p0, p1, p2, p3, n=96):
    out = []
    for i in range(n + 1):
        t = i / n
        a, b, c, d = (1-t)**3, 3*(1-t)**2*t, 3*(1-t)*t*t, t**3
        out.append((a*p0[0]+b*p1[0]+c*p2[0]+d*p3[0], a*p0[1]+b*p1[1]+c*p2[1]+d*p3[1]))
    return out

def geometry(tail):
    """Points en coordonnées libres : cerf-volant incliné vers le haut à droite, queue en S."""
    ang = math.radians(32)
    def r(x, y):
        return (x * math.cos(ang) - y * math.sin(ang), x * math.sin(ang) + y * math.cos(ang))
    T, Rr, B, L, X = r(0, -1.0), r(0.70, -0.25), r(0, 1.0), r(-0.70, -0.25), r(0, -0.25)
    pts = {"T": T, "R": Rr, "B": B, "L": L, "X": X}
    curve = []
    if tail:
        bx, by = B
        curve = bez3(B, (bx + 0.10, by + 0.55), (bx - 0.75, by + 0.30), (bx - 0.80, by + 0.85))
    return pts, curve

def draw(size, background=True, tail=None):
    if tail is None:
        tail = size >= 40
    ss = 8
    S = size * ss
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    if background:
        m = 0.03 * S if size >= 32 else 0
        d.rounded_rectangle([m, m, S - m, S - m], radius=0.22 * S, fill=BG)
    pts, curve = geometry(tail)
    allp = list(pts.values()) + curve
    xs, ys = [p[0] for p in allp], [p[1] for p in allp]
    w, h = max(xs) - min(xs), max(ys) - min(ys)
    box = (0.66 if tail else 0.74) * S
    k = box / max(w, h)
    ox = S / 2 - (min(xs) + w / 2) * k
    oy = S / 2 - (min(ys) + h / 2) * k
    U = lambda p: (ox + p[0] * k, oy + p[1] * k)
    T, R, B, L, X = (U(pts[n]) for n in "TRBLX")
    d.polygon([T, L, X], fill=FG)
    d.polygon([T, R, X], fill=ACCENT)
    d.polygon([X, R, B], fill=FG)
    d.polygon([X, L, B], fill=FG)
    gap = max(0.022 * S, 1.0 * ss)
    d.line([T, B], fill=BG, width=int(gap))
    d.line([L, R], fill=BG, width=int(gap))
    if curve:
        c = [U(p) for p in curve]
        # queue effilée : de plus en plus fine
        n = len(c) - 1
        for i in range(n):
            wd = 0.019 * S * (1 - 0.5 * i / n)
            d.line([c[i], c[i + 1]], fill=FG, width=max(1, int(wd)))
            d.ellipse([c[i][0] - wd / 2, c[i][1] - wd / 2, c[i][0] + wd / 2, c[i][1] + wd / 2], fill=FG)
    return img.resize((size, size), Image.LANCZOS)

SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]

def preview(out):
    """Planche d'aperçu de toutes les tailles, sur fond clair et sombre."""
    sheet = Image.new("RGBA", (1100, 560), (255, 255, 255, 255))
    sheet.alpha_composite(draw(512), (10, 20))
    sheet.alpha_composite(draw(256), (540, 20))
    x = 810
    for s in [128, 64]:
        sheet.alpha_composite(draw(s), (x, 20)); x += s + 12
    x = 540
    for s in [48, 32, 24, 16]:
        sheet.alpha_composite(draw(s), (x, 300)); x += s + 20
    dark = Image.new("RGBA", (520, 120), (32, 32, 32, 255))
    xx = 10
    for s in [64, 48, 32, 24, 16]:
        dark.alpha_composite(draw(s), (xx, 20)); xx += s + 20
    sheet.alpha_composite(dark, (540, 420))
    sheet.save(out + "/preview.png")

if __name__ == "__main__":
    # python assets/icon/generate_icon.py assets/icon [--preview]   (nécessite Pillow)
    out = sys.argv[1] if len(sys.argv) > 1 else "."
    draw(256).save(out + "/kite-256.png")
    # Marque seule (sans fond ni queue) : en-tête de la barre latérale, déjà sur fond sombre.
    draw(64, background=False, tail=False).save(out + "/kite-mark-64.png")
    frames = [draw(s) for s in SIZES]
    frames[-1].save(out + "/kite.ico", sizes=[(s, s) for s in SIZES], append_images=frames[:-1])
    if "--preview" in sys.argv:
        preview(out)
    print("ok")
