# Site

The Wye website: a landing page built from the repository's own files and published to GitHub Pages.

- `pages/`: HTML pages whose directives pull from the repository (`src/render/directive.rs` lists them), and `site.css`, which the build writes into each page, minified.
- `assets/`: files copied into `out/`, scripts without their comments (`src/main.rs`): `site.js` (the copy buttons, and the hero's start), `strings.js` (the worker that draws the hero's strings), the icon (`icon-fork.svg`, with PNG copies for browsers without SVG icons: 32 px on transparency, and 180 px on the tile's colour for iOS, each a Chromium screenshot of the SVG), and the fonts: Wye Text (iA Writer Quattro S) and Wye Mono (IBM Plex Mono), each subset and renamed, under the SIL Open Font License in `assets/fonts/`. The subset keeps printable ASCII and `U+A0 B5 B7 D7 AB BB B0 B1 A9 2013 2014 2018 2019 201C 201D 2022 2026 2192 2248 2264 2265`, no hinting, and only the `kern` feature: `pyftsubset FONT.woff2 --flavor=woff2 --unicodes=... --layout-features=kern --no-hinting --name-IDs='*'`. A character outside it falls back to the next font for that character only.
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
