# Site design

The rules the Wye website is designed and reviewed against. Each one comes from Wye itself, from its [principles](../docs/product.md#principles), its name, or its behaviour: the site should behave like the product it describes. The landing page is built ([README.md](README.md)); the docs pages are planned.

## Scope

The landing page comes first. Type, colour, tables, and code blocks are chosen for long reading as well, because the next pages are the user docs rendered from the repository: [guide.md](../docs/guide.md) and the configuration reference, [examples/wye.toml](../examples/wye.toml). Until those pages exist, the landing page links to them on GitHub. The other docs serve contributors and stay on GitHub. A page is rendered from its source file, never copied into the site.

## Principles

### Show the real artifact

*From native and transparent.* The evidence is Wye's own output, verbatim: `wye report`, `wye explain`, a usage record, a real `wye.toml`. No mock dashboards, no pictures of "AI", no fake terminal chrome (window dots, typing animations of commands nobody ran). Output blocks get the same typographic care as headings: aligned columns, real widths, syntax colour marked by hand.

Check: every code and output block is taken from the repository, or from the binary, at build time.

### Claims at their real size

*From report, don't rewrite.* Say what is built, what is unverified, and what is not built, at the same size as the claims; caveats are not grey footnotes. Lab numbers carry the machine and the method and link to [benchmarks.md](../docs/benchmarks.md), including where Wye loses. No adjectives that cannot be measured, and no logos, testimonials, or star counts. The voice is the docs' voice: short sentences, numbers with units.

Check: every claim links to the doc section that backs it, and nothing contradicts [roadmap.md](../docs/roadmap.md#status).

### Never the slow part

*From fast and light.* HTML and CSS; a script may add conveniences, such as copy buttons and a play button for a reader who asked for reduced motion, but every page reads and works without it. At most three self-hosted, subset font files (a reading face, and two weights of one face for everything else), with metric-matched fallbacks so nothing moves when they load. Starting budget: the landing page under 150 KB over the wire, fonts included.

Check: `site/check.sh` fails a page over budget, the way the benchmark gate fails a slower gateway.

### Keep no state

*From request-local and stateless.* No cookies, analytics, consent banner, popups, or requests to another origin. Every heading has a stable anchor, so a link says exactly where it points. Pages work with scripts off, in reader mode, and in print.

Check: loading any page makes no request outside the site.

### Install first

*From easy to use.* The first screen says what Wye is in one sentence and shows the commands that run it: the install line, `wye init`, `wye service install`, `wye claude`. The commands are the call to action, ready to copy, not a button to another page.

Check: a Claude Code user can start Wye from the first screen without scrolling.

### Draw the mechanism

*From the name.* A wye is a Y-shaped fitting or junction: one line splitting into branches. Wye's request path is that shape: one client, several accounts, a session held on one account by hashing, and failover switched only before the first response byte, the way points are set before the train reaches them. Diagrams show this and nothing decorative, drawn from [architecture.md](../docs/architecture.md#claude-across-providers-and-accounts). Colour marks meaning (route, upstream, failover, refusal), never ornament. The motif must not slide into signal-light green on black or a hairline broadsheet layout; both are on the list below.

Check: a diagram changes in the same commit as the behaviour it shows.

### Motion shows what happens over time

*From streaming and failover.* Wye's work happens in time: a request takes the junction, a refusal switches the points before the first byte, a stream comes back event by event. Motion is how the page shows that, replayed from a recorded run, never with invented timing. Each page gets one orchestrated sequence; apart from it, motion only answers the reader (opening, expanding, copying, the position in the table of contents), and one blinking cursor sits where a terminal would leave it. No fade-and-rise entrances, no hover effects on every block, no parallax.

CSS only, no animation library: transitions, keyframes, `@starting-style`, scroll-driven animations, and view transitions, each degrading to a still page where a browser lacks it. Animate `transform` and `opacity`, so motion stays off the main thread. Under `prefers-reduced-motion` the sequence becomes a still diagram that says the same thing, with a play button that runs it once when the reader asks.

Check: with motion off, the page says everything it says with motion on.

### One bold thing

*From one responsibility.* Each page has one job and one memorable element, and everything around it stays quiet. The quality floor is never announced: usable at 320 px wide, visible keyboard focus, text contrast that meets WCAG AA.

Check: the review of each page names its job and its one bold element.

## Direction: terminal route

The route map, drawn as a terminal draws it. Chosen on 2026-11-01 from three proposals (route map, piping schematic, annotated output), then moved to a terminal ("hacker") style the same day at the user's request. Wye's config already names routes and upstreams, and Wye is a command-line tool whose output is the evidence on the page; so the page's structure is set in monospace (headings, code, output, numbers, the diagram, the table of contents), its diagram is box-drawing characters on a character grid, its table of contents prints like `tree`, and the junction on the first screen is the wye. Running text is set in a typewriter face made for reading. The page is dark only and monochrome, white on black, whatever the system's theme; colour is kept for meaning: amber is the failover route and red a refusal, and neither appears anywhere else. Near-black with one accent is on the list below; amber here is not an accent but the second route's colour. The user chose dark only on 2026-11-01 (the "Wye site v2" design canvas) and the monochrome palette, the reading face, the spacing, and the mark on 2026-11-02 (the "Wye site v3" canvas).

| Token | Value | Use (WCAG contrast against `paper`) |
| --- | --- | --- |
| `paper` | `#050505` | Page ground |
| `ground` | `#101010` | Code and output |
| `rule` | `#232323` | Dividers |
| `ink` | `#F5F5F5` | Headings, emphasis, the first route, links, the mark (18.7:1) |
| `text` | `#D0D0D0` | Running text (13.2:1) |
| `graphite` | `#909090` | Secondary text (6.4:1) |
| `branch` | `#F0B43C` | The failover route, nothing else (11.0:1) |
| `signal` | `#FF6474` | Refusals (429, 529), nothing else (7.1:1) |

White and amber are the only route colours; they differ in lightness as well as hue, so they stay apart under any colour blindness. A refusal is also spelled out (`x`, `429`), never shown by colour alone.

Type: Wye Text for running text (paragraphs, lists, captions, the compare table), Wye Mono for everything else: SemiBold for headings, leads, and bold; Regular for code, output, numbers, the diagram, the table of contents, navigation, and buttons. Wye Text is iA Writer Quattro S, a Plex Mono derivative whose wide and narrow letters read like prose and still look typed; on a phone it fits about 40 characters a line where Plex Mono fits 33. It is subset to Latin and punctuation and renamed, since "iA Writer" and "Plex" are Reserved Font Names under its SIL Open Font License (`assets/fonts/OFL-iA-Writer.txt`). Wye Mono is IBM Plex Mono, subset to Latin, box drawing, and block elements, and renamed the same way (`assets/fonts/OFL-Plex.txt`); it carries the box-drawing characters the diagram and the rail need. Its box-drawing glyphs are 1.3 em tall, so the diagram and the rail use that line height and their strokes join from line to line. Text is 17 px (16 px on a phone), with a measure of 64 zeros of Wye Text, about 70 characters.

Spacing groups what belongs together: a stop's lead sits close above its text, a block of code, output, or numbers stands a little apart from the text around it and further from what follows than from what leads into it, and a dashed rule separates stops.

Diagrams: box-drawing characters on a grid of whole character cells, the route in `ink`, the failover branch in `branch`, stations as `o`, the request as `█`, labels in `graphite`. Moving parts jump a cell at a time (`steps()` in `ch` and `lh` units) on a `paper` background, so a moving glyph replaces the character under it. Only real upstream ids are drawn.

Mark: the junction as the diagram draws it, one line forking in two, in `ink` on a `ground` tile with a lighter edge, so it reads at 16 px in a browser tab on light and dark bars alike.

Layout: the page follows one request through Wye. Its sections are stops in the order a request meets them (client, gate, model, account, stream, record), then the sections off the route (how it compares, the config, status, what Wye will not do), each headed by a lowercase name after a `#` that links to it. On wide screens a rail beside them is the table of contents, printed like `tree`, and the stop being read is selected in inverse video through scroll-driven animations; on narrow screens the route runs down the stops' left edge as a line. Each stop shows its item of README's "What it does", placed by the item's bold lead; an item the page does not place appears at the end of the config section, so a new one is never dropped.

## Defaults we do not use

These appear on generated pages whatever the subject. None is used unless chosen on purpose, with the reason written down:

- Indigo or purple gradients, glows, glass panels.
- Content cut into identical rounded cards, an icon on top of each, the same soft shadow under all.
- Eyebrow labels over headings, all-caps labels, small monospace data labels, `→` appended to links, one accented word in a headline.
- Cream with a terracotta accent (near `#D97757`, Anthropic's own accent; on a gateway for Claude it reads as borrowed branding), and near-black with one acid-green or vermilion accent.
- Inter, Geist, Space Grotesk, Instrument Serif, Fraunces, or JetBrains Mono as a reflex.
- A badge above the headline, stat banners, 01 / 02 / 03 markers on content that is not a sequence, logo strips.

## Review

- Plan before code with Anthropic's `frontend-design` skill: 4–6 named colours, type roles, ASCII wireframes. Review the plan against these principles and revise any part that would come out the same for any other gateway.
- Run `npx impeccable detect` on the built page and check it against Vercel's web interface guidelines, locally; neither is a site dependency. Run Impeccable from `site/`, never from the repository root: it treats this file as its design file and keeps its own files (`PRODUCT.md`, `.impeccable/`) beside it.
- Review every animation with the `review-animations` skill, and its cost with `animation-performance`.
- Look at screenshots at phone and desktop widths before each commit that changes the look.
