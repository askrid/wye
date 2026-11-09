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

// The headline's lines as strings. Nothing moves by itself: a string the
// pointer crosses is plucked, rings, nudges its neighbours, and sends a
// request on along its route, and the dot glows as the request passes.
// It all lives on the GPU, drawn by a worker (assets/strings.js) on a canvas
// handed to it, so neither the context nor a frame takes this thread's time.
// Frames run only while something the reader started still moves, on the
// low-power GPU, and never under prefers-reduced-motion, out of view, or in
// a hidden tab; without WebGL in a worker the drawing stays still. Nothing
// is started until motion may play and a mouse or trackpad could pluck (a
// touch screen never does), and then in an idle moment after load.
const period = document.querySelector(".period");
if (period) {
  const reduce = matchMedia("(prefers-reduced-motion: reduce)");
  const mouse = matchMedia("(any-hover: hover) and (any-pointer: fine)");
  let started = false;
  const begin = () => {
    if (started || reduce.matches || !mouse.matches) return;
    started = true;
    (window.requestIdleCallback || setTimeout)(() => hero(period, reduce));
  };
  reduce.addEventListener("change", begin);
  mouse.addEventListener("change", begin);
  begin();
}
function hero(period, reduce) {
  const canvas = document.createElement("canvas");
  if (!canvas.transferControlToOffscreen) return;
  canvas.setAttribute("aria-hidden", "true");
  period.append(canvas);
  const style = getComputedStyle(period);
  const rgb = (name) => [1, 3, 5].map((i) => parseInt(style.getPropertyValue(name).trim().slice(i, i + 2), 16) / 255);
  const offscreen = canvas.transferControlToOffscreen();
  const worker = new Worker("assets/strings.js");
  worker.postMessage({
    canvas: offscreen,
    paths: [...period.querySelectorAll(".strands path"), period.querySelector(".thread")].map((el) => el.getAttribute("d")),
    ink: rgb("--ink"),
    accent: rgb("--accent"),
  }, [offscreen]);
  // The pointer's place in the drawing's units. The canvas's box is read at
  // each move, about one a frame (browsers align moves to frames), so no
  // shift of the page leaves it stale.
  const pluck = (event) => {
    if (event.pointerType !== "mouse") return;
    const box = canvas.getBoundingClientRect();
    worker.postMessage({ at: [((event.clientX - box.left) / box.width) * 1500, ((event.clientY - box.top) / box.height) * 600] });
  };
  // A pointer that left the window, or a page that scrolled under it, starts
  // afresh, so its next move does not pluck every string in between.
  const forget = () => worker.postMessage({ forget: true });
  let running = false, visible = false;
  // The pointer is heard only while the strings may move; when they may
  // not, the worker stills them.
  const update = () => {
    const on = visible && !document.hidden && !reduce.matches;
    if (on === running) return;
    running = on;
    const listen = on ? "addEventListener" : "removeEventListener";
    window[listen]("pointermove", pluck, { passive: true });
    window[listen]("scroll", forget, { passive: true });
    document.documentElement[listen]("pointerleave", forget);
    if (!on) worker.postMessage({ stop: true });
  };
  new ResizeObserver(([entry]) => worker.postMessage({ size: [Math.round(entry.contentRect.width), Math.round(entry.contentRect.height)] })).observe(canvas);
  new IntersectionObserver(([entry]) => {
    visible = entry.isIntersecting;
    update();
  }).observe(canvas);
  document.addEventListener("visibilitychange", update);
  reduce.addEventListener("change", update);
}
