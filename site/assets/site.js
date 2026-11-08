// Copy buttons on command and config blocks, a convenience the page works
// without: with scripts off the text stays selectable. A shell block is
// copied without its comments: zsh, macOS's shell, runs a pasted "# ..." as
// arguments unless interactivecomments is set.
const uncommented = (text) =>
  text
    .split("\n")
    .map((line) => line.replace(/\s+#\s.*$/, ""))
    .filter((line) => !/^\s*#/.test(line))
    .join("\n");
for (const pre of document.querySelectorAll('pre.code[data-lang="bash"], pre.code[data-lang="toml"]')) {
  const wrap = document.createElement("div");
  wrap.className = "code-wrap";
  pre.before(wrap);
  wrap.append(pre);
  const button = document.createElement("button");
  button.type = "button";
  button.className = "copy";
  button.textContent = "[copy]";
  // What happened, for screen readers, which don't announce a button whose
  // text changes.
  const status = document.createElement("span");
  status.className = "visually-hidden";
  status.setAttribute("role", "status");
  let reset;
  button.addEventListener("click", async () => {
    clearTimeout(reset);
    try {
      await navigator.clipboard.writeText(pre.dataset.lang === "bash" ? uncommented(pre.innerText) : pre.innerText);
      button.textContent = "[copied]";
      status.textContent = "Copied";
    } catch {
      // No clipboard access: select the text instead.
      getSelection().selectAllChildren(pre);
      button.textContent = "[selected]";
      status.textContent = "Selected; copy it with your keyboard";
    }
    reset = setTimeout(() => {
      button.textContent = "[copy]";
      status.textContent = "";
    }, 1500);
  });
  wrap.append(button, status);
}

// The headline's lines as strings: each sways a little, slowly, and one the
// pointer crosses is plucked, rings, and settles; both ends stay put, like a
// string's. Ambient only: it never moves the page, runs only while the hero
// is in view, and stays still under prefers-reduced-motion; the m key turns
// it on or off either way, remembered on this browser.
const strings = document.querySelector(".period svg");
if (strings) {
  const reduce = matchMedia("(prefers-reduced-motion: reduce)");
  let chosen = null;
  try {
    chosen = localStorage.getItem("wye-motion");
  } catch {}
  const lines = [...strings.querySelectorAll(".lines path, .thread")].map((el, i) => {
    const d = el.getAttribute("d");
    const [x0, y0, x1, y1, x2, y2, x3, y3, tail] = d.match(/-?[\d.]+/g).map(Number);
    // Points along the line at rest, to find where the pointer crosses it.
    const at = [];
    for (let k = 0; k <= 24; k++) {
      const t = k / 24, u = 1 - t;
      at.push([
        u * u * u * x0 + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t * t * t * x3,
        u * u * u * y0 + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t * t * t * y3,
      ]);
    }
    return { el, d, x0, y0, x1, y1, x2, y2, x3, y3, tail, at, amp: 0, since: 0,
      phase: i * 2.399, period: 9000 + (i % 7) * 900, thread: el.classList.contains("thread") };
  });
  const yAt = (line, x) => {
    const at = line.at;
    if (x < at[0][0] || x > at[at.length - 1][0]) return null;
    for (let k = 1; k < at.length; k++) {
      if (x <= at[k][0]) {
        const [ax, ay] = at[k - 1], [bx, by] = at[k];
        return ay + ((by - ay) * (x - ax)) / (bx - ax || 1);
      }
    }
    return null;
  };
  let running = false, visible = true, frame = 0, last = 0, previous = null;
  const draw = (now) => {
    frame = requestAnimationFrame(draw);
    if (now - last < 32) return; // about 30 frames a second is enough
    last = now;
    for (const line of lines) {
      const sway = line.thread ? 0 : 1.4 * Math.sin((2 * Math.PI * now) / line.period + line.phase);
      let ring = 0;
      if (line.amp) {
        const age = now - line.since;
        ring = line.amp * Math.exp(-age / 750) * Math.sin((2 * Math.PI * 2.6 * age) / 1000);
        if (age > 3500) line.amp = 0;
      }
      const dy = sway + ring;
      line.el.setAttribute("d", `M${line.x0} ${line.y0}C${line.x1} ${(line.y1 + dy).toFixed(1)} ${line.x2} ${(line.y2 + dy).toFixed(1)} ${line.x3} ${line.y3}${line.tail ? "H" + line.tail : ""}`);
      if (!line.thread) line.el.style.opacity = line.amp ? (0.2 + Math.min(0.45, Math.abs(ring) / 10)).toFixed(2) : "";
    }
  };
  const pluck = (event) => {
    if (event.pointerType !== "mouse") return;
    const point = new DOMPoint(event.clientX, event.clientY).matrixTransform(strings.getScreenCTM().inverse());
    if (previous) {
      const speed = Math.min(8, Math.max(2, Math.abs(point.y - previous.y) * 0.6));
      for (const line of lines) {
        const before = yAt(line, previous.x), after = yAt(line, point.x);
        if (before === null || after === null) continue;
        if (Math.sign(previous.y - before) !== Math.sign(point.y - after)) {
          line.amp = speed * Math.sign(point.y - previous.y || 1);
          line.since = performance.now();
        }
      }
    }
    previous = point;
  };
  const update = () => {
    const on = visible && !document.hidden && (chosen ? chosen === "on" : !reduce.matches);
    if (on === running) return;
    running = on;
    if (on) {
      frame = requestAnimationFrame(draw);
      addEventListener("pointermove", pluck, { passive: true });
    } else {
      cancelAnimationFrame(frame);
      removeEventListener("pointermove", pluck);
      previous = null;
      for (const line of lines) {
        line.amp = 0;
        line.el.setAttribute("d", line.d);
        line.el.style.opacity = "";
      }
    }
  };
  new IntersectionObserver(([entry]) => {
    visible = entry.isIntersecting;
    update();
  }).observe(strings);
  document.addEventListener("visibilitychange", update);
  reduce.addEventListener("change", update);
  addEventListener("keydown", (event) => {
    if (event.code !== "KeyM" || event.metaKey || event.ctrlKey || event.altKey || event.target.closest("input, textarea, [contenteditable]")) return;
    chosen = running ? "off" : "on";
    try {
      localStorage.setItem("wye-motion", chosen);
    } catch {}
    update();
  });
  update();
}
