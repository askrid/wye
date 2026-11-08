// Copy buttons on command and config blocks, a convenience the page works
// without: with scripts off the text stays selectable. A shell block is
// copied without its comments: zsh, macOS's shell, runs a pasted "# ..." as
// arguments unless interactivecomments is set.
const uncommented = (text) =>
  text
    .split("\n")
    .filter((line) => !/^\s*#/.test(line))
    .map((line) => line.replace(/\s+#\s.*$/, ""))
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

// The headline's lines as strings: one the pointer crosses is plucked,
// rings, and settles, both ends still, like a string's. The ringing copy is
// drawn on a canvas over the drawing, only while a line rings: at rest the
// script does nothing (the sway is CSS). Plucks stop under
// prefers-reduced-motion, out of view, and in a hidden tab.
const period = document.querySelector(".period");
if (period) {
  const reduce = matchMedia("(prefers-reduced-motion: reduce)");
  const canvas = document.createElement("canvas");
  canvas.setAttribute("aria-hidden", "true");
  period.append(canvas);
  const ctx = canvas.getContext("2d");
  const lines = [...period.querySelectorAll(".strands path, .thread")].map((el) => {
    const [x0, y0, x1, y1, x2, y2, x3, y3, tail] = el.getAttribute("d").match(/-?[\d.]+/g).map(Number);
    // Points along the line at rest, to find where the pointer crosses it.
    const at = [];
    for (let k = 0; k <= 24; k++) {
      const t = k / 24, u = 1 - t;
      at.push([
        u * u * u * x0 + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t * t * t * x3,
        u * u * u * y0 + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t * t * t * y3,
      ]);
    }
    if (tail) at.push([tail, y3]);
    return { x0, y0, x1, y1, x2, y2, x3, y3, tail, at, amp: 0, since: 0, thread: el.classList.contains("thread") };
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
  let running = false, visible = false, frame = 0, previous = null, scale = 1, inkFade, accentFade;
  // One canvas pixel per CSS pixel, the drawing's box; each line is stroked
  // with the drawing's fade, from the dot to nothing before the edges.
  const fit = () => {
    const box = canvas.getBoundingClientRect();
    canvas.width = Math.round(box.width);
    canvas.height = Math.round(box.height);
    scale = canvas.width / 1500;
    const style = getComputedStyle(period);
    const fade = (colour) => {
      const gradient = ctx.createRadialGradient(850, 300, 0, 850, 300, 640);
      gradient.addColorStop(0.35, colour);
      gradient.addColorStop(1, "transparent");
      return gradient;
    };
    inkFade = fade(style.getPropertyValue("--ink").trim());
    accentFade = fade(style.getPropertyValue("--accent").trim());
  };
  // A plucked line rings at 7 Hz, brightens, and settles within a second and
  // a half; when none rings, the canvas is cleared and the loop stops.
  const draw = (now) => {
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.setTransform(scale, 0, 0, scale, 0, 0);
    let ringing = false;
    for (const line of lines) {
      if (!line.amp) continue;
      const age = now - line.since;
      if (age > 1600) {
        line.amp = 0;
        continue;
      }
      ringing = true;
      const dy = line.amp * Math.exp(-age / 350) * Math.sin((2 * Math.PI * 7 * age) / 1000);
      ctx.strokeStyle = line.thread ? accentFade : inkFade;
      ctx.lineWidth = line.thread ? 1.6 : 0.9;
      ctx.globalAlpha = line.thread ? 1 : Math.min(0.7, 0.2 + Math.abs(dy) / 6);
      ctx.beginPath();
      ctx.moveTo(line.x0, line.y0);
      ctx.bezierCurveTo(line.x1, line.y1 + dy, line.x2, line.y2 + dy, line.x3, line.y3);
      if (line.tail) ctx.lineTo(line.tail, line.y3);
      ctx.stroke();
    }
    frame = ringing ? requestAnimationFrame(draw) : 0;
  };
  const pluck = (event) => {
    if (event.pointerType !== "mouse") return;
    const box = canvas.getBoundingClientRect();
    if (!box.width) return;
    const point = { x: ((event.clientX - box.left) / box.width) * 1500, y: ((event.clientY - box.top) / box.height) * 600 };
    if (previous) {
      const speed = Math.min(8, Math.max(2, Math.abs(point.y - previous.y) * 0.6));
      let plucked = false;
      for (const line of lines) {
        const before = yAt(line, previous.x), after = yAt(line, point.x);
        if (before === null || after === null) continue;
        if (Math.sign(previous.y - before) !== Math.sign(point.y - after)) {
          line.amp = speed * Math.sign(point.y - previous.y || 1);
          line.since = performance.now();
          plucked = true;
        }
      }
      if (plucked && !frame) frame = requestAnimationFrame(draw);
    }
    previous = point;
  };
  // A pointer that left the window, or a page that scrolled under it, starts
  // afresh, so its next move does not pluck every line in between.
  const forget = () => {
    previous = null;
  };
  const update = () => {
    const on = visible && !document.hidden && !reduce.matches;
    if (on === running) return;
    running = on;
    if (on) {
      fit();
      addEventListener("pointermove", pluck, { passive: true });
      addEventListener("scroll", forget, { passive: true });
      document.documentElement.addEventListener("pointerleave", forget);
    } else {
      cancelAnimationFrame(frame);
      frame = 0;
      removeEventListener("pointermove", pluck);
      removeEventListener("scroll", forget);
      document.documentElement.removeEventListener("pointerleave", forget);
      previous = null;
      for (const line of lines) line.amp = 0;
      ctx.clearRect(0, 0, canvas.width, canvas.height);
    }
  };
  new ResizeObserver(() => running && fit()).observe(canvas);
  new IntersectionObserver(([entry]) => {
    visible = entry.isIntersecting;
    update();
  }).observe(canvas);
  document.addEventListener("visibilitychange", update);
  reduce.addEventListener("change", update);
  update();
}
