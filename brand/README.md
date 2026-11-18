# TripleTrail logo

The mark is three stepping stones along a dotted trail: from the first triple to where you want to
go. The last stone is amber, the colour the website and the map use for routes.

| File | Use it for |
| --- | --- |
| `tripletrail-logo.svg` | The logo on light backgrounds: documents, slides, the website header. |
| `tripletrail-logo-dark.svg` | The logo on dark backgrounds. |
| `tripletrail-mark.svg` | The mark alone, 48 px and larger: app icons, avatars, social profiles. |
| `tripletrail-mark-small.svg` | The mark at 32 px and smaller: favicons, toolbar icons. No trail, bigger stones. |
| `tripletrail-mark-mono.svg` | One colour (it takes the surrounding text colour): stamps, engraving, fax-grade print. |
| `favicon.ico` | Old browsers and tools that only look for `/favicon.ico` (16, 32 and 48 px). |
| `fonts/` | Barlow SemiBold, the font of the word mark, with its licence (SIL OFL 1.1): `build.py` reads it. |
| `png/` | Rendered copies: icons at 16 to 512 px, the logo at 800 px wide, and a 1200 × 630 link preview card. |

The website (its own repository, `src/assets/`) and the server's web UI (`crates/tripletrail-server/ui/icon.svg`)
use copies of these files.

## Colours

| Name | Hex | Where |
| --- | --- | --- |
| Matdata blue | `#0563bb` | The tile, "Trail" on light backgrounds |
| Amber | `#f2b300` | The last stone |
| Ink | `#1c2633` | "Triple" on light backgrounds |
| Blue on dark | `#2f8cff` | "Trail" on dark backgrounds |
| Light on dark | `#edf0f4` | "Triple" on dark backgrounds |

The wordmark is Barlow SemiBold (SIL Open Font License 1.1), turned into outlines so the files need
no font. Keep clear space around the logo of at least half the tile's width, and don't recolour,
stretch, rotate or add effects to it.

## Changing the logo

The SVGs are generated, so change `build.py`, not the files:

```sh
pip install fonttools brotli uharfbuzz
python3 brand/build.py                      # the SVGs
NODE_PATH=$(npm root -g) node brand/render.mjs   # the PNGs and favicon.ico (needs Playwright)
```

Then copy the changed files to the website and the server UI as listed above.

## Trademark

TripleTrail and the stepping-stones mark are trademarks of Matdata. The Apache License 2.0 that
covers the code does not grant permission to use them (section 6), apart from saying truthfully
where the software comes from.
