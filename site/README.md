# Site

The Wye website: a landing page built from the repository's own files and published to GitHub Pages. How it looks and why: [DESIGN.md](DESIGN.md).

| Path | Contents |
| --- | --- |
| `pages/` | HTML pages with directives that pull from the repository; `src/render.rs` lists them |
| `assets/` | The stylesheet, the copy-button script, the icon, and the fonts (Departure Mono and Overpass Mono, each under the SIL Open Font License in `assets/fonts/`) |
| `src/` | The generator: renders `pages/` into `out/`, copies `assets/`, and checks the result |
| `check.sh` | `cargo fmt --check`, clippy, tests, the build, and the page budget |

Build it and look at it:

```bash
site/check.sh
python3 -m http.server --directory site/out 8000   # then open http://localhost:8000
```

A page never holds a second copy of anything the repository already says: numbers come from `bench/baseline.json`, the version and license from the manifests, and commands, output, and status from `README.md`. The build fails when one of them is missing, when a local link or anchor does not resolve, or when a page would load anything from another origin.

The site is its own Cargo workspace, so its dependencies stay out of the gateway's lockfile and a site change never runs the gateway's benchmark gate. The pre-commit hook runs `site/check.sh` instead for a commit that touches the site, and also for one that changes a file the site reads. `.github/workflows/site.yml` runs the same check on pull requests and deploys `site/out` to GitHub Pages from `main` ([development.md](../docs/development.md#continuous-integration)).
