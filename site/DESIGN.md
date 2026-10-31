# Site design

The rules the Wye website is designed and reviewed against. Each one comes from Wye itself, from its [principles](../docs/product.md#principles), its name, or its behaviour: the site should behave like the product it describes. Proposal: no page is built yet.

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

*From fast and light.* HTML and CSS; no page needs a script to be read or used. At most two self-hosted, subset font files, with metric-matched fallbacks so nothing moves when they load. Starting budget: the landing page under 150 KB over the wire, fonts included.

Check: `site/check.sh` fails a page over budget, the way the benchmark gate fails a slower gateway.

### Keep no state

*From request-local and stateless.* No cookies, analytics, consent banner, popups, or requests to another origin. Every heading has a stable anchor, so a link says exactly where it points. Pages work with scripts off, in reader mode, and in print.

Check: loading any page makes no request outside the site.

### Install first

*From easy to use.* The first screen says what Wye is in one sentence and shows the commands that run it: the install line, `wye init`, `wye service install`, `wye claude`. The commands are the call to action, ready to copy, not a button to another page.

Check: a Claude Code user can start Wye from the first screen without scrolling.

### Draw the mechanism

*From the name.* A wye is a Y-shaped fitting or junction: one line splitting into branches. Wye's request path is that shape: one client, several accounts, a session held on one account by hashing, and failover switched only before the first response byte, the way points are set before the train reaches them. Diagrams show this and nothing decorative, drawn from [architecture.md](../docs/architecture.md#claude-across-providers-m4). Colour marks meaning (route, upstream, failover, refusal), never ornament. The motif must not slide into signal-light green on black or a hairline broadsheet layout; both are on the list below.

Check: a diagram changes in the same commit as the behaviour it shows.

### Motion shows what happens over time

*From streaming and failover.* Wye's work happens in time: a request takes the junction, a refusal switches the points before the first byte, a stream comes back event by event. Motion is how the page shows that, replayed from a recorded run, never with invented timing. Each page gets one orchestrated sequence; apart from it, motion only answers the reader (opening, expanding, copying). No fade-and-rise entrances, no hover effects on every block, no parallax.

CSS only, no animation library: transitions, keyframes, `@starting-style`, scroll-driven animations, and view transitions, each degrading to a still page where a browser lacks it. Animate `transform` and `opacity`, so motion stays off the main thread. Under `prefers-reduced-motion` the sequence becomes a still diagram that says the same thing.

Check: with motion off, the page says everything it says with motion on.

### One bold thing

*From one responsibility.* Each page has one job and one memorable element, and everything around it stays quiet. The quality floor is never announced: usable at 320 px wide, visible keyboard focus, text contrast that meets WCAG AA, light and dark.

Check: the review of each page names its job and its one bold element.

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
- Run `npx impeccable detect` on the built page and check it against Vercel's web interface guidelines, locally; neither is a site dependency.
- Review every animation with the `review-animations` skill, and its cost with `animation-performance`.
- Look at screenshots at phone and desktop widths, light and dark, before each commit that changes the look.
