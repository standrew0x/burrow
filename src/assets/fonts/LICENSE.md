# Bundled fonts

Both families are vendored rather than fetched, because the app's
content-security policy is `default-src 'self'` and a remote font request is
refused at runtime — the interface would silently fall back to Georgia and
system-ui. Redistribution is permitted: both are SIL Open Font License 1.1.

| File | Family | Source |
|---|---|---|
| `fraunces-latin.woff2`, `fraunces-latin-ext.woff2` | Fraunces (variable, `opsz` 9–144, `wght` 100–900) | https://fonts.google.com/specimen/Fraunces |
| `instrument-sans-latin.woff2`, `instrument-sans-latin-ext.woff2` | Instrument Sans (variable, `wght` 400–700) | https://fonts.google.com/specimen/Instrument+Sans |

Latin and latin-extended subsets only. Latin-ext is included because post text
routinely carries accented characters, and a missing subset shows as a fallback
glyph mid-word rather than as an obvious failure.

Full licence text: https://openfontlicense.org
