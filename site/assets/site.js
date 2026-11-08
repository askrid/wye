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
// rings, and settles, both ends still, like a string's. The lines live on
// the GPU: their shapes are uploaded once, and each frame sends only how far
// each line is displaced, so one draw call bends, brightens, and fades them
// all (only ringing lines show). Nothing runs at rest (the sway is CSS);
// plucks stop under prefers-reduced-motion, out of view, and in a hidden
// tab, and without WebGL the drawing just sways.
const period = document.querySelector(".period");
const canvas = document.createElement("canvas");
const gl = period && canvas.getContext("webgl", { antialias: true, premultipliedAlpha: true });
if (gl) {
  const reduce = matchMedia("(prefers-reduced-motion: reduce)");
  canvas.setAttribute("aria-hidden", "true");
  period.append(canvas);
  const paths = [...period.querySelectorAll(".strands path"), period.querySelector(".thread")];
  // Each line as 40 segments; a vertex is x, y, how much a pluck moves it
  // (the curve's middle control points' weight, 3t(1-t)), and its line.
  const vertices = [];
  const lines = paths.map((el, i) => {
    const [x0, y0, x1, y1, x2, y2, x3, y3, tail] = el.getAttribute("d").match(/-?[\d.]+/g).map(Number);
    const at = [];
    for (let k = 0; k <= 40; k++) {
      const t = k / 40, u = 1 - t;
      at.push([
        u * u * u * x0 + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t * t * t * x3,
        u * u * u * y0 + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t * t * t * y3,
        3 * t * u,
      ]);
    }
    if (tail) at.push([tail, y3, 0]);
    for (let k = 1; k < at.length; k++) vertices.push(...at[k - 1], i, ...at[k], i);
    return { at, amp: 0, since: 0 };
  });
  const thread = lines.length - 1;
  const strandVertices = vertices.length / 4 - (lines[thread].at.length - 1) * 2;
  const shader = (type, source) => {
    const s = gl.createShader(type);
    gl.shaderSource(s, source);
    gl.compileShader(s);
    return s;
  };
  const program = gl.createProgram();
  gl.attachShader(program, shader(gl.VERTEX_SHADER, `
    attribute vec4 a;
    uniform vec4 u_dy[50];
    uniform float u_solid;
    varying vec2 v_pos;
    varying float v_alpha;
    void main() {
      vec4 four = u_dy[int(a.w / 4.0)];
      float k = mod(a.w, 4.0);
      float dy = k < 0.5 ? four.x : k < 1.5 ? four.y : k < 2.5 ? four.z : four.w;
      vec2 p = vec2(a.x, a.y + a.z * dy);
      v_pos = p;
      v_alpha = dy == 0.0 ? 0.0 : u_solid > 0.5 ? 1.0 : min(0.7, 0.2 + abs(dy) / 6.0);
      gl_Position = vec4(p.x / 750.0 - 1.0, 1.0 - p.y / 300.0, 0.0, 1.0);
    }`));
  gl.attachShader(program, shader(gl.FRAGMENT_SHADER, `
    precision mediump float;
    uniform vec3 u_colour;
    varying vec2 v_pos;
    varying float v_alpha;
    void main() {
      float a = v_alpha * clamp((640.0 - length(v_pos - vec2(850.0, 300.0))) / 416.0, 0.0, 1.0);
      gl_FragColor = vec4(u_colour * a, a);
    }`));
  gl.bindAttribLocation(program, 0, "a");
  gl.linkProgram(program);
  gl.useProgram(program);
  gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(vertices), gl.STATIC_DRAW);
  gl.enableVertexAttribArray(0);
  gl.vertexAttribPointer(0, 4, gl.FLOAT, false, 0, 0);
  gl.enable(gl.BLEND);
  gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
  const where = (name) => gl.getUniformLocation(program, name);
  const [uDy, uSolid, uColour] = ["u_dy", "u_solid", "u_colour"].map(where);
  const dys = new Float32Array(200);
  const rgb = (name) => {
    const hex = getComputedStyle(period).getPropertyValue(name).trim().slice(1);
    return [0, 2, 4].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
  };
  let ink, accent, running = false, visible = false, frame = 0, previous = null;
  // One canvas pixel per CSS pixel: the lines draw a pixel wide, as the
  // drawing's 0.9-unit strokes do.
  const fit = () => {
    const box = canvas.getBoundingClientRect();
    canvas.width = Math.round(box.width);
    canvas.height = Math.round(box.height);
    gl.viewport(0, 0, canvas.width, canvas.height);
    ink = rgb("--ink");
    accent = rgb("--accent");
  };
  // A plucked line rings at 7 Hz, brightens, and settles within a second and
  // a half; when none rings, the canvas is cleared and the loop stops.
  const draw = (now) => {
    let ringing = false;
    lines.forEach((line, i) => {
      const age = now - line.since;
      if (line.amp && age > 1600) line.amp = 0;
      dys[i] = line.amp ? line.amp * Math.exp(-age / 350) * Math.sin((2 * Math.PI * 7 * age) / 1000) || 1e-6 : 0;
      if (line.amp) ringing = true;
    });
    gl.clearColor(0, 0, 0, 0);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.uniform4fv(uDy, dys);
    gl.uniform1f(uSolid, 0);
    gl.uniform3fv(uColour, ink);
    gl.drawArrays(gl.LINES, 0, strandVertices);
    gl.uniform1f(uSolid, 1);
    gl.uniform3fv(uColour, accent);
    gl.drawArrays(gl.LINES, strandVertices, vertices.length / 4 - strandVertices);
    frame = ringing ? requestAnimationFrame(draw) : 0;
  };
  // Where the pointer crosses a line at rest, by the line's own points.
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
      gl.clear(gl.COLOR_BUFFER_BIT);
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
