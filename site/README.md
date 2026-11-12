# Site

The Wye website: a landing page built from the repository's own files and published to GitHub Pages.

- `pages/`: HTML pages whose directives pull from the repository (`src/render/directive.rs` lists them), and `site.css`, which the build writes into each page, minified.
- `assets/`: files copied into `out/`, scripts without their comments (`src/main.rs`): `site.js` (the copy buttons, and the hero's start), `strings.js` (the worker that draws the hero's strings), the mark (`icon-dither.svg`, a pixel Y with one amber arm, and PNG copies, each a Chromium screenshot of the SVG: 32 px on transparency, the favicon every browser uses, as browsers take the last icon link they can draw and Safari 26 and later did not show the SVG; and 180 px on the tile's colour for iOS; a new mark takes a new file name, since browsers keep an icon long after it changes), and the fonts: Wye Text (iA Writer Quattro S) and Wye Mono (IBM Plex Mono), each subset and renamed, under the SIL Open Font License in `assets/fonts/`. The subset keeps printable ASCII and `U+A0 B5 B7 D7 AB BB B0 B1 A9 2013 2014 2018 2019 201C 201D 2022 2026 2192 2248 2264 2265`, no hinting, and only the `kern` feature: `pyftsubset FONT.woff2 --flavor=woff2 --unicodes=... --layout-features=kern --no-hinting --name-IDs='*'`. A character outside it falls back to the next font for that character only.
- `share/card.html`: the source of `assets/share.png`, the image a link preview shows, and the repository's social preview on GitHub (uploaded in its settings). To redraw it, serve `site/` (`python3 -m http.server --directory site`) and screenshot `share/card.html` with any Chromium: `--headless --screenshot --window-size=1280,640`.
- `src/`: the generator: renders `pages/` into `out/`, copies `assets/`, and checks the result.
- `check.sh`: `cargo fmt --check`, clippy, tests, the build, and the page budget.

Build it and look at it:

```bash
site/check.sh
python3 -m http.server --directory site/out 8000   # then open http://localhost:8000
```

A page holds no second copy of text the repository already says: benchmark numbers come from the tables in `README.md` and `docs/benchmarks.md`, the version and license from the manifests, and commands, output, and status from `README.md` and `docs/operations.md`. The figures that are drawings (the client APIs, the token, the upgrade check, the upstream sessions) are written by hand from real output and checked against the code when it changes. The build fails when a source is missing, when a local link, anchor, or fragment does not resolve, when a link into the repository names a file it does not have, or when a page or a script would load anything from another origin.

The site is its own Cargo workspace, so its dependencies stay out of the gateway's lockfile and a site change never runs the gateway's commit gate. The pre-commit hook runs `site/check.sh` instead for a commit that touches the site, and also for one that changes a file the site reads. `.github/workflows/site.yml` runs the same check on pull requests and deploys `site/out` to GitHub Pages from `main` ([development.md](../docs/development.md#continuous-integration)).

## Open findings

Found in the review round of 2026-11-10 and not fixed, with why, so the next round starts here:

- `src/check.rs` reads only double-quoted `href` and `src`, and `url(…)` in CSS: a single-quoted or unquoted attribute, `srcset`, `ping`, or `@import` would pass. The page has none; raw HTML in a rendered README section could.
- Code blocks that only wrap (the hero's, `.long-lines`) are still focusable, and the ones that scroll have no accessible name. Fixing it needs `{{fence}}` to know where its block is placed.
- The worker's messages are told apart by which field they carry, not a `type`; the anchors map is keyed by a `FILE#slug` string; the page builds `{{repo}}/blob/{{commit}}/…` URLs itself, which `Sources::files_url` already knows. Small, and nothing has gone wrong yet.
- Not checked in a browser: whether Safari fires `pointerleave` on the root when the mouse leaves the window, and whether a Windows tap moves the hidden mouse position (both would make one stray sweep of plucks).
