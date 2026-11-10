// Copy buttons on the blocks the build marks (data-copy), a convenience the
// page works without: with scripts off the text stays selectable. A shell
// block is copied without the comments the build marked (.c): zsh, macOS's
// shell, runs a pasted "# ..." as arguments unless interactivecomments is
// set.
const copied = (pre) => {
  if (pre.dataset.copy !== "shell") return pre.textContent;
  const commands = pre.cloneNode(true);
  for (const comment of commands.querySelectorAll(".c")) comment.remove();
  return commands.textContent.split("\n").map((line) => line.trimEnd()).filter(Boolean).join("\n");
};
for (const pre of document.querySelectorAll("pre.code[data-copy]")) {
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
    // Emptied first, so a second copy is announced again.
    status.textContent = "";
    try {
      await navigator.clipboard.writeText(copied(pre));
      button.textContent = "[copied]";
      status.textContent = "Copied";
    } catch {
      // No clipboard access: select the text instead.
      getSelection().selectAllChildren(pre);
      button.textContent = "[selected]";
      status.textContent = "Selected; copy it with your keyboard";
    }
    clearTimeout(reset);
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
// is started until motion may play, a mouse or trackpad could pluck (a touch
// screen never does), and the colours are the page's (no forced contrast
// theme), and then in an idle moment once the page is parsed (after load
// where the browser cannot say when it is idle).
const period = document.querySelector(".period");
if (period) {
  const reduce = matchMedia("(prefers-reduced-motion: reduce)");
  const mouse = matchMedia("(any-hover: hover) and (any-pointer: fine) and (forced-colors: none)");
  const idle = window.requestIdleCallback
    || ((f) => (document.readyState === "complete" ? setTimeout(f) : addEventListener("load", () => setTimeout(f), { once: true })));
  let started = false;
  const begin = () => {
    if (started || reduce.matches || !mouse.matches) return;
    started = true;
    // Asked again when the moment comes: either may have changed meanwhile.
    idle(() => {
      if (reduce.matches || !mouse.matches) started = false;
      else hero(period, reduce);
    });
  };
  reduce.addEventListener("change", begin);
  mouse.addEventListener("change", begin);
  begin();
}
function hero(period, reduce) {
  const canvas = document.createElement("canvas");
  if (!canvas.transferControlToOffscreen) return;
  // A worker the page may not start (a file:// page) leaves the drawing as
  // it is, before the canvas is handed over.
  let worker;
  try {
    worker = new Worker("assets/strings.js");
  } catch {
    return;
  }
  canvas.setAttribute("aria-hidden", "true");
  period.append(canvas);
  const style = getComputedStyle(period);
  const rgb = (name) => [1, 3, 5].map((i) => parseInt(style.getPropertyValue(name).trim().slice(i, i + 2), 16) / 255);
  const offscreen = canvas.transferControlToOffscreen();
  worker.postMessage({
    canvas: offscreen,
    paths: [...period.querySelectorAll(".strands path"), period.querySelector(".thread")].map((el) => el.getAttribute("d")),
    ink: rgb("--ink"),
    accent: rgb("--accent"),
  }, [offscreen]);
  // The pointer's place in the drawing's units. The canvas's box is read at
  // each move, about one a frame (browsers align moves to frames), so no
  // shift of the page leaves it stale. A pen or a touch never plucks, and
  // the mouse's next move starts afresh from wherever it is then.
  let mouseLast = true;
  const pluck = (event) => {
    if (event.pointerType !== "mouse") {
      if (mouseLast) forget();
      mouseLast = false;
      return;
    }
    mouseLast = true;
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
    window[listen]("pointerdown", pluck, { passive: true });
    window[listen]("scroll", forget, { passive: true });
    document.documentElement[listen]("pointerleave", forget);
    if (!on) worker.postMessage({ stop: true });
  };
  new ResizeObserver(([entry]) => worker.postMessage({ size: [Math.round(entry.contentRect.width), Math.round(entry.contentRect.height)] })).observe(canvas);
  // The latest entry: a fast scroll can deliver several at once.
  new IntersectionObserver((entries) => {
    visible = entries[entries.length - 1].isIntersecting;
    update();
  }).observe(canvas);
  document.addEventListener("visibilitychange", update);
  reduce.addEventListener("change", update);
}
