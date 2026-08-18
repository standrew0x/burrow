"""Draws the Burrow app icon and writes every size the app ships.

Painted directly rather than rasterised from an SVG: there is no SVG renderer on
this machine that does not need a native cairo build, and drawing it here means
the master is reproducible from source with nothing but PIL and numpy.

    python generate.py <variant> [outdir]

Variants:
    stack   three plates, fanned, brightening toward the viewer
    arch    a tunnel mouth with wall thickness

Everything is authored in a 512-unit design space and drawn at 8x, then
downsampled with LANCZOS. Drawing at final size leaves the squircle and the
plate corners visibly stepped.
"""

import sys
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

DESIGN = 512
SS = 8
N = DESIGN * SS

PNG_SIZES = [32, 128, 256, 512, 1024]
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

AMBER_LIT = (0xFF, 0xF3, 0xDE)
AMBER = (0xFF, 0xB0, 0x4A)
AMBER_DEEP = (0xE8, 0x7A, 0x18)


def d(v: float) -> int:
    return int(round(v * SS))


def squircle_mask(size: int, exponent: float = 5.0) -> Image.Image:
    """A superellipse, not a rounded rectangle.

    |x|^n + |y|^n = 1 with n=5 is the continuous-curvature corner modern
    platform icons use. A large `rx` on a rect is visibly more abrupt.
    """
    t = (np.arange(size, dtype=np.float64) + 0.5) / size * 2.0 - 1.0
    x = np.abs(t)[None, :] ** exponent
    y = np.abs(t)[:, None] ** exponent
    return Image.fromarray((((x + y) <= 1.0) * 255).astype(np.uint8), mode="L")


def vgrad(h: int, w: int, stops) -> Image.Image:
    pos = np.array([s[0] for s in stops], dtype=np.float64)
    cols = np.array([s[1] for s in stops], dtype=np.float64)
    y = np.linspace(0.0, 1.0, h)
    out = np.stack([np.interp(y, pos, cols[:, c]) for c in range(3)], axis=-1)
    return Image.fromarray(
        np.repeat(out[:, None, :], w, axis=1).astype(np.uint8), mode="RGB")


def radial_alpha(size, cx, cy, radius, stops) -> Image.Image:
    ys, xs = np.mgrid[0:size, 0:size]
    xs = (xs + 0.5) / size
    ys = (ys + 0.5) / size
    r = np.sqrt((xs - cx) ** 2 + (ys - cy) ** 2) / radius
    pos = np.array([s[0] for s in stops], dtype=np.float64)
    val = np.array([s[1] for s in stops], dtype=np.float64)
    a = np.interp(np.clip(r, 0, 1), pos, val)
    return Image.fromarray((a * 255).astype(np.uint8), mode="L")


def tinted(size, rgb, alpha) -> Image.Image:
    layer = Image.new("RGBA", (size, size), tuple(rgb) + (0,))
    layer.putalpha(alpha)
    return layer


def fill_shape(mask: Image.Image, stops, y0: int, h: int) -> Image.Image:
    """Fills an arbitrary mask with a vertical gradient spanning y0..y0+h."""
    size = mask.size[0]
    full = Image.new("RGB", (size, size), stops[-1][1])
    full.paste(vgrad(h, size, stops), (0, y0))
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(full, (0, 0), mask)
    return out


def rounded_mask(size, box, radius) -> Image.Image:
    x0, y0, w, h = (d(v) for v in box)
    m = Image.new("L", (size, size), 0)
    ImageDraw.Draw(m).rounded_rectangle(
        [x0, y0, x0 + w, y0 + h], radius=d(radius), fill=255)
    return m


def base_tile() -> Image.Image:
    """Dark body, restrained glow, gloss. Shared by every variant."""
    body = vgrad(N, N, [
        (0.00, (0x35, 0x2E, 0x3C)),
        (0.38, (0x1A, 0x16, 0x22)),
        (1.00, (0x0A, 0x08, 0x0D)),
    ]).convert("RGBA")

    # Restrained on purpose. The first attempt ran this at 0.95 peak over a 0.78
    # radius and the bottom half of the tile turned into an orange smear that
    # swallowed the glyph. It only has to lift the tile off a black taskbar.
    glow = radial_alpha(N, 0.5, 1.06, 0.52,
                        [(0.0, 0.42), (0.45, 0.16), (1.0, 0.0)])
    body.alpha_composite(tinted(N, (0xFF, 0x9A, 0x30), glow))

    gloss_a = np.clip(np.interp(np.linspace(0, 1, N),
                                [0.0, 0.55, 1.0], [0.16, 0.02, 0.0]), 0, 1)
    body.alpha_composite(tinted(N, (255, 255, 255), Image.fromarray(
        np.repeat((gloss_a * 255).astype(np.uint8)[:, None], N, axis=1), mode="L")))
    return body


def glyph_stack(body: Image.Image) -> None:
    """Three plates, fanned left-to-right so they read as a shuffled stack
    rather than as three centred bars — which at 32px is a hamburger menu."""
    plates = [
        # x,    y,   w,   h,  radius, stops
        ((188, 118, 196, 40), 11, [(0, (0x6E, 0x4C, 0x28)), (1, (0x4A, 0x30, 0x14))]),
        ((140, 178, 244, 46), 12, [(0, (0xB8, 0x7C, 0x38)), (1, (0x8A, 0x4E, 0x18))]),
        ((92, 248, 328, 152), 20, [(0.00, AMBER_LIT), (0.48, AMBER), (1.00, AMBER_DEEP)]),
    ]
    for box, radius, stops in plates:
        body.alpha_composite(
            fill_shape(rounded_mask(N, box, radius), stops, d(box[1]), d(box[3])))

    # Lit top edge on the front plate; without it the stack looks printed on.
    hl = Image.new("RGBA", (N, N), (0, 0, 0, 0))
    ImageDraw.Draw(hl).rounded_rectangle(
        [d(92), d(248), d(92 + 328), d(248 + 12)], radius=d(6),
        fill=(255, 255, 255, int(255 * 0.6)))
    body.alpha_composite(hl)


def glyph_arch(body: Image.Image) -> None:
    """A tunnel with wall thickness, so you see into it rather than at it."""
    outer = Image.new("L", (N, N), 0)
    dr = ImageDraw.Draw(outer)
    dr.pieslice([d(112), d(120), d(400), d(408)], 180, 360, fill=255)
    dr.rectangle([d(112), d(264), d(400), d(408)], fill=255)

    inner = Image.new("L", (N, N), 0)
    di = ImageDraw.Draw(inner)
    di.pieslice([d(196), d(204), d(316), d(324)], 180, 360, fill=255)
    di.rectangle([d(196), d(264), d(316), d(412)], fill=255)

    ring = Image.fromarray(
        np.clip(np.asarray(outer).astype(np.int16) - np.asarray(inner), 0, 255)
        .astype(np.uint8), mode="L")

    body.alpha_composite(fill_shape(
        ring, [(0.00, AMBER_LIT), (0.45, AMBER), (1.00, AMBER_DEEP)],
        d(120), d(288)))


def build(variant: str) -> Image.Image:
    body = base_tile()
    {"stack": glyph_stack, "arch": glyph_arch}[variant](body)

    img = Image.new("RGBA", (N, N), (0, 0, 0, 0))
    img.paste(body, (0, 0), squircle_mask(N))

    # Rim light: the other half of the dark-ground problem.
    inner = squircle_mask(N)
    ring = Image.fromarray(
        np.clip(np.asarray(inner).astype(np.int16)
                - np.asarray(inner.filter(ImageFilter.MinFilter(11))), 0, 255)
        .astype(np.uint8), mode="L")
    img.alpha_composite(tinted(N, (255, 255, 255),
                               ring.point(lambda v: int(v * 0.26))))
    return img


def main() -> None:
    variant = sys.argv[1] if len(sys.argv) > 1 else "stack"
    outdir = Path(sys.argv[2] if len(sys.argv) > 2 else ".")
    outdir.mkdir(parents=True, exist_ok=True)

    master = build(variant)
    print(f"{variant}: master {master.size[0]}x{master.size[1]}")
    for s in PNG_SIZES:
        master.resize((s, s), Image.LANCZOS).save(outdir / f"{variant}-{s}.png")
    master.resize((256, 256), Image.LANCZOS).save(
        outdir / f"{variant}.ico", sizes=[(s, s) for s in ICO_SIZES])
    print(f"  wrote {len(PNG_SIZES)} png + ico ({len(ICO_SIZES)} resolutions)")


if __name__ == "__main__":
    main()
