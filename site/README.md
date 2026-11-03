# Site

The Wye website: a landing page built from the repository's own files and published to GitHub Pages.

- `pages/`: HTML pages whose directives pull from the repository (`src/render.rs` lists the directives), and `site.css`, which the build writes into each page.
- `assets/`: files served as they are: the copy-button script, the icon (`icon-fork.svg`, with PNG copies for browsers without SVG icons: 32 px on transparency, and 180 px on the tile's colour for iOS, each a Chromium screenshot of the SVG), and the fonts: Wye Text (iA Writer Quattro S) and Wye Mono (IBM Plex Mono), each subset and renamed, under the SIL Open Font License in `assets/fonts/`.
- `share/card.html`: the source of `assets/share.png`, the image a link preview shows. To redraw it, serve `site/` (`python3 -m http.server --directory site`) and screenshot `share/card.html` with any Chromium: `--headless --screenshot --window-size=1200,630`.
- `src/`: the generator: renders `pages/` into `out/`, copies `assets/`, and checks the result.
- `check.sh`: `cargo fmt --check`, clippy, tests, the build, and the page budget.

Build it and look at it:

```bash
site/check.sh
python3 -m http.server --directory site/out 8000   # then open http://localhost:8000
```

A page never holds a second copy of anything the repository already says: benchmark numbers come from the tables in `README.md` and `docs/benchmarks.md`, the version and license from the manifests, and commands, output, and status from `README.md`. The build fails when one of them is missing, when a local link or anchor does not resolve, or when a page would load anything from another origin.

The site is its own Cargo workspace, so its dependencies stay out of the gateway's lockfile and a site change never runs the gateway's commit gate. The pre-commit hook runs `site/check.sh` instead for a commit that touches the site, and also for one that changes a file the site reads. `.github/workflows/site.yml` runs the same check on pull requests and deploys `site/out` to GitHub Pages from `main` ([development.md](../docs/development.md#continuous-integration)).
