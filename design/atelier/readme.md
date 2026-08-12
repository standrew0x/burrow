# Atelier

A warm-paper design system for a reference library.

## The premise

A reference library is a working surface, not a showroom. Everything strong on
screen should be the imagery pinned to it — so the chrome is paper, ink,
hairlines, and a lot of air.

Every hue sits between 20° and 60°: paper, olive, clay. That is the whole
palette, and it is narrow on purpose. This interface frames other people's
photographs, and a saturated or cool interface colour competes with them.

## What makes it Atelier

Change any one of these and it stops being this system:

- **Air separates, not rules.** Spacing runs at density 1.25 and dividers are a
  14% ink hairline. There is exactly one heavy rule (`.hr-strong`) and it is for
  the top of a section.
- **2px radius.** Not zero, not round — a trimmed edge rather than a moulded
  corner.
- **Nothing floats.** Elevation is near-absent. Only `.dialog` gets real depth,
  because it is the only surface that blocks.
- **Two families, one rule.** Fraunces carries anything a person wrote —
  headings, captions, notes. Instrument Sans carries the interface. Machine
  output — filenames, dimensions, hashes — goes to the mono face. If a machine
  produced the string, it is not set in the serif.
- **One filled button per view.** Everything else is a hairline or nothing.
- **Clay is only for destruction.** Introducing a red would make four hues.

## Image treatment: none

Deliberately. This system dresses a library that is *searched by colour* — every
reference carries an extracted palette, and the palette is the index. A
grayscale or duotone treatment would look coherent and break the tool. Images
are framed instead: `.plate` gives a hairline and a paper mat.

For the same reason `.tile-palette` is always visible rather than revealed on
hover. Hiding it would hide the index.

## Relationship to Modernist

The class vocabulary is deliberately identical — `.btn`, `.input`, `.card`,
`.tag`, `.nav`, `.table`, `.dialog`, `.seg`, `.radio` — so the same markup can
wear either system by swapping `styles.css`. Where Modernist separates with
weight, black rules and a shouting red, Atelier separates with space, hairlines
and a muted olive.

## Files

- `styles.css` — the source of truth. Retune here.
- `theme.json` — token summary.
- `foundations/` — colour ramps, type scale.
- `components/` — buttons, forms, navigation, dialog, and the app-specific
  reference tile.

## Fonts and content-security policy

`styles.css` here pulls Fraunces and Instrument Sans from Google Fonts over
`@import`, which is fine for these preview pages. Any host with a strict
`default-src 'self'` policy will refuse that request and fall back silently to
Georgia and system-ui — the design still *renders*, which is what makes the
failure easy to miss.

Burrow, which is a Tauri shell and has exactly that policy, therefore vendors
both families instead: `src/assets/fonts/*.woff2`, latin and latin-extended
subsets, referenced from `@font-face` with relative URLs so they resolve as
`'self'`. 168 KB for all four files. Both faces are OFL, so redistributing them
is permitted.

Do the same in any other app that adopts this system. Checking is one line in
the console: if `document.fonts.check('16px Fraunces')` is false, the serif
never arrived and the interface is quietly wearing Georgia.
