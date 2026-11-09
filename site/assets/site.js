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
// It all lives on the GPU: the strings' shapes are uploaded once, and a
// frame sends each string's displacement and each request's place, then
// draws twice. Frames run only while something the reader started still
// moves, on the low-power GPU, and never under prefers-reduced-motion,
// out of view, or in a hidden tab; without WebGL the drawing stays still.
// Nothing is built until motion may play and a mouse or trackpad could
// pluck (a touch screen never does), and then in an idle moment after load:
// a first visit's shaders take the GPU over 100 ms to compile.
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
  const gl = canvas.getContext("webgl", { antialias: true, premultipliedAlpha: true, powerPreference: "low-power" });
  if (!gl) return;
  canvas.setAttribute("aria-hidden", "true");
  period.append(canvas);
  const paths = [...period.querySelectorAll(".strands path"), period.querySelector(".thread")];
  // Each string as 40 segments, and an out string's straight tail as one
  // more; a vertex is x, y, how much a pluck moves it
  // (the curve's middle control points' weight, 3t(1-t)), and its string.
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
  // The drawing's order: 100 strings in, then 99 out (in's order, less the
  // one that fails over), then the thread, that one's way out.
  const thread = lines.length - 1;
  const route = (i) => (i === 59 ? thread : 100 + (i < 59 ? i : i - 1));
  const strandVertices = vertices.length / 4 - (lines[thread].at.length - 1) * 2;
  const program = (vertex, fragment) => {
    const p = gl.createProgram();
    for (const [type, source] of [[gl.VERTEX_SHADER, vertex], [gl.FRAGMENT_SHADER, fragment]]) {
      const s = gl.createShader(type);
      gl.shaderSource(s, source);
      gl.compileShader(s);
      gl.attachShader(p, s);
    }
    gl.bindAttribLocation(p, 0, "a");
    gl.linkProgram(p);
    return p;
  };
  // The fade both programs share: from near the dot to nothing before the
  // drawing's edges.
  const fade = "clamp((640.0 - length(v_pos - vec2(850.0, 300.0))) / 416.0, 0.0, 1.0)";
  const strings = program(`
    attribute vec4 a;
    uniform vec4 u_dy[50];
    uniform vec4 u_glow[50];
    uniform float u_solid;
    varying vec2 v_pos;
    varying float v_alpha;
    void main() {
      int i = int(a.w / 4.0);
      float k = mod(a.w, 4.0);
      vec4 m = vec4(k < 0.5, k >= 0.5 && k < 1.5, k >= 1.5 && k < 2.5, k >= 2.5);
      vec2 p = vec2(a.x, a.y + a.z * dot(u_dy[i], m));
      float glow = dot(u_glow[i], m);
      v_pos = p;
      v_alpha = u_solid > 0.5 ? min(1.0, glow * 1.8) : glow;
      // A string at rest is not drawn at all: all its points fall outside
      // the view, so the GPU fills no pixel for it.
      gl_Position = glow == 0.0 ? vec4(2.0, 2.0, 2.0, 1.0) : vec4(p.x / 750.0 - 1.0, 1.0 - p.y / 300.0, 0.0, 1.0);
    }`, `
    precision mediump float;
    uniform vec3 u_colour;
    varying vec2 v_pos;
    varying float v_alpha;
    void main() {
      float a = v_alpha * ${fade};
      gl_FragColor = vec4(u_colour * a, a);
    }`);
  // A pulse is a soft point of light: x, y, brightness, size.
  const pulses = program(`
    attribute vec4 a;
    varying vec2 v_pos;
    varying float v_alpha;
    void main() {
      v_pos = a.xy;
      v_alpha = a.z;
      gl_PointSize = a.w;
      gl_Position = vec4(a.x / 750.0 - 1.0, 1.0 - a.y / 300.0, 0.0, 1.0);
    }`, `
    precision mediump float;
    uniform vec3 u_colour;
    varying vec2 v_pos;
    varying float v_alpha;
    void main() {
      vec2 c = gl_PointCoord * 2.0 - 1.0;
      float a = v_alpha * exp(-4.0 * dot(c, c)) * ${fade};
      gl_FragColor = vec4(u_colour * a, a);
    }`);
  const lineBuffer = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, lineBuffer);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(vertices), gl.STATIC_DRAW);
  const pulseBuffer = gl.createBuffer();
  const pulseData = new Float32Array((48 * 10 + 1) * 4);
  gl.enableVertexAttribArray(0);
  gl.enable(gl.BLEND);
  // The shaders' inputs, found once both programs have linked.
  let u = null;
  const dys = new Float32Array(200);
  const glows = new Float32Array(200);
  const rgb = (name) => {
    const hex = getComputedStyle(period).getPropertyValue(name).trim().slice(1);
    return [0, 2, 4].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
  };
  let ink, accent, halo = 0, running = false, visible = false, lost = false, frame = 0, last = 0;
  // The pointer: its latest place, read once a frame, the place read the frame
  // before, and the canvas's box, measured again only after a scroll or a
  // resize.
  let box = null, pointerX = 0, pointerY = 0, pending = false, previousX = 0, previousY = 0, havePrevious = false;
  // One canvas pixel per CSS pixel: a ringing string draws a pixel wide, as
  // the drawing's 0.9-unit strokes do, and the pulses are soft anyway.
  const fit = () => {
    box = canvas.getBoundingClientRect();
    canvas.width = Math.round(box.width);
    canvas.height = Math.round(box.height);
    gl.viewport(0, 0, canvas.width, canvas.height);
    ink = rgb("--ink");
    accent = rgb("--accent");
  };
  // Requests: a string in, then its string out, i the string it is on. In,
  // they gather speed into the dot; out, they ease off. At most 48 at once
  // (and 6 new a frame, as sense launches no more).
  const flight = [];
  const IN = 1500, OUT = 1900;
  const launch = (i, out, t) => {
    if (flight.length < 48) flight.push({ i, out, t });
  };
  // Where the pointer crosses a string at rest, by the string's own points.
  const yAt = (line, x) => {
    const at = line.at;
    if (x < at[0][0] || x > at[at.length - 1][0]) return null;
    for (let k = 1; k < at.length; k++) {
      if (x <= at[k][0]) {
        const a = at[k - 1], b = at[k];
        return a[1] + ((b[1] - a[1]) * (x - a[0])) / (b[0] - a[0] || 1);
      }
    }
    return null;
  };
  // A plucked string's neighbours pick up a faint ring, 40 ms later per
  // string away, unless they already ring louder.
  const ring = (j, amp, now) => {
    const line = lines[j];
    if (!line || j === thread || Math.abs(line.amp) > Math.abs(amp)) return;
    line.amp = amp;
    line.since = now;
  };
  // Once a frame, every string the pointer crossed since the last frame is
  // plucked, its swing set by the pointer's speed. Only a slow touch, three
  // strings or fewer, wakes the neighbours; in a sweep they are crossed
  // anyway. Each plucked string sends a request on from where it was
  // plucked, up to six a frame, spread over the strings crossed.
  const crossed = [];
  const sense = (now) => {
    pending = false;
    if (!box) box = canvas.getBoundingClientRect();
    if (!box.width) return;
    const x = ((pointerX - box.left) / box.width) * 1500, y = ((pointerY - box.top) / box.height) * 600;
    if (havePrevious) {
      crossed.length = 0;
      for (let i = 0; i < lines.length; i++) {
        const before = yAt(lines[i], previousX), after = yAt(lines[i], x);
        if (before !== null && after !== null && Math.sign(previousY - before) !== Math.sign(y - after)) crossed.push(i);
      }
      const amp = Math.min(5, Math.max(1.5, Math.abs(y - previousY) * 0.4)) * Math.sign(y - previousY || 1);
      const every = Math.ceil(crossed.length / 6);
      for (let c = 0; c < crossed.length; c++) {
        const i = crossed[c], line = lines[i];
        line.amp = amp;
        line.since = now;
        if (crossed.length <= 3) {
          ring(i - 1, amp * 0.35, now + 40);
          ring(i + 1, amp * 0.35, now + 40);
          ring(i - 2, amp * 0.12, now + 80);
          ring(i + 2, amp * 0.12, now + 80);
        }
        if (c % every) continue;
        const at = line.at, t = (x - at[0][0]) / (at[at.length - 1][0] - at[0][0]);
        if (i < 100) launch(i, false, Math.sqrt(Math.max(t, 0)));
        else launch(i, true, 1 - Math.sqrt(1 - Math.min(t, 1)));
      }
    }
    previousX = x;
    previousY = y;
    havePrevious = true;
  };
  // Writes a request's streak into the buffer at point n: a head and nine
  // fading points close behind it. Returns the next free point.
  const streak = (p, n) => {
    const at = lines[p.i].at, end = at.length - 1;
    const ease = p.out ? 1 - (1 - p.t) * (1 - p.t) : p.t * p.t;
    for (let k = 0; k < 10; k++) {
      const f = Math.min(Math.max(ease - k * 0.006, 0), 1) * end, j = Math.min(Math.floor(f), end - 1), r = f - j;
      const a = at[j], b = at[j + 1], o = n * 4;
      pulseData[o] = a[0] + (b[0] - a[0]) * r;
      pulseData[o + 1] = a[1] + (b[1] - a[1]) * r + (a[2] + (b[2] - a[2]) * r) * dys[p.i];
      pulseData[o + 2] = (1 - k / 10) ** 2 * 0.8;
      pulseData[o + 3] = 10 - k * 0.7;
      n++;
    }
    return n;
  };
  const draw = (now) => {
    const dt = Math.min(now - (last || now), 50);
    last = now;
    if (pending) sense(now);
    // Strings: a plucked one rings at 3 Hz, its swing dying over half a
    // second; its copy glows in over 60 ms and out over 700, to nothing.
    let ringing = false;
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i], age = now - line.since;
      dys[i] = glows[i] = 0;
      if (!line.amp) continue;
      ringing = true;
      if (age < 0) continue;
      const glow = 0.55 * (1 - Math.exp(-Math.abs(line.amp) / 3)) * (1 - Math.exp(-age / 60)) * Math.exp(-age / 700);
      if (age > 300 && glow < 0.002) {
        line.amp = 0;
        continue;
      }
      dys[i] = line.amp * Math.exp(-age / 520) * Math.sin((2 * Math.PI * 3 * age) / 1000);
      glows[i] = glow;
    }
    // The dot carries the traffic: each request through it brightens a soft
    // halo, which dies away over 400 ms.
    halo *= Math.exp(-dt / 400);
    for (let k = flight.length - 1; k >= 0; k--) {
      const p = flight[k];
      p.t += dt / (p.out ? OUT : IN);
      if (p.t < 1) continue;
      if (p.out) flight.splice(k, 1);
      else {
        p.out = true;
        p.i = route(p.i);
        p.t = 0;
        halo = Math.min(1, halo + 0.18);
      }
    }
    pulseData[0] = 900;
    pulseData[1] = 300;
    pulseData[2] = halo * 0.35;
    pulseData[3] = 70;
    let n = 1;
    for (const p of flight) if (p.i !== thread) n = streak(p, n);
    const accentAt = n;
    for (const p of flight) if (p.i === thread) n = streak(p, n);
    gl.clear(gl.COLOR_BUFFER_BIT);
    // The ringing strings.
    gl.useProgram(strings);
    gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
    gl.bindBuffer(gl.ARRAY_BUFFER, lineBuffer);
    gl.vertexAttribPointer(0, 4, gl.FLOAT, false, 0, 0);
    gl.uniform4fv(u.dy, dys);
    gl.uniform4fv(u.glow, glows);
    gl.uniform1f(u.solid, 0);
    gl.uniform3fv(u.lineColour, ink);
    gl.drawArrays(gl.LINES, 0, strandVertices);
    gl.uniform1f(u.solid, 1);
    gl.uniform3fv(u.lineColour, accent);
    gl.drawArrays(gl.LINES, strandVertices, vertices.length / 4 - strandVertices);
    // The requests, added light on light.
    gl.useProgram(pulses);
    gl.blendFunc(gl.ONE, gl.ONE);
    gl.bindBuffer(gl.ARRAY_BUFFER, pulseBuffer);
    // Fresh storage each frame, so the GPU never makes the page wait for it
    // to finish reading last frame's.
    gl.bufferData(gl.ARRAY_BUFFER, pulseData.subarray(0, n * 4), gl.STREAM_DRAW);
    gl.vertexAttribPointer(0, 4, gl.FLOAT, false, 0, 0);
    gl.uniform3fv(u.pulseColour, ink);
    gl.drawArrays(gl.POINTS, 0, accentAt);
    gl.uniform3fv(u.pulseColour, accent);
    gl.drawArrays(gl.POINTS, accentAt, n - accentAt);
    // Go on while anything still moves; otherwise the frame just drawn is
    // empty, and the loop stops until the pointer moves again.
    if (flight.length || halo > 0.003 || ringing) frame = requestAnimationFrame(draw);
    else frame = last = 0;
  };
  // The pointer only notes where it is and asks for a frame.
  const pluck = (event) => {
    if (event.pointerType !== "mouse") return;
    pointerX = event.clientX;
    pointerY = event.clientY;
    pending = true;
    if (!frame) frame = requestAnimationFrame(draw);
  };
  // A pointer that left the window, or a page that scrolled under it, starts
  // afresh, so its next move does not pluck every string in between.
  const forget = () => {
    havePrevious = pending = false;
    box = null;
  };
  const update = () => {
    if (!u) return;
    const on = visible && !lost && !document.hidden && !reduce.matches;
    if (on === running) return;
    running = on;
    if (on) {
      fit();
      addEventListener("pointermove", pluck, { passive: true });
      addEventListener("scroll", forget, { passive: true });
      document.documentElement.addEventListener("pointerleave", forget);
    } else {
      cancelAnimationFrame(frame);
      frame = last = 0;
      halo = 0;
      removeEventListener("pointermove", pluck);
      removeEventListener("scroll", forget);
      document.documentElement.removeEventListener("pointerleave", forget);
      forget();
      flight.length = 0;
      for (const line of lines) line.amp = 0;
      if (!lost) gl.clear(gl.COLOR_BUFFER_BIT);
    }
  };
  // A lost GPU context (a reset, a sleep) stops the motion; the drawing stays.
  canvas.addEventListener("webglcontextlost", (event) => {
    event.preventDefault();
    lost = true;
    update();
  });
  new ResizeObserver(() => running && fit()).observe(canvas);
  new IntersectionObserver(([entry]) => {
    visible = entry.isIntersecting;
    update();
  }).observe(canvas);
  document.addEventListener("visibilitychange", update);
  reduce.addEventListener("change", update);
  // The programs link on the GPU's own time where the browser allows it;
  // asking for an input before then would make the page wait for them.
  const parallel = gl.getExtension("KHR_parallel_shader_compile");
  const linked = () => {
    if (parallel && ![strings, pulses].every((p) => gl.getProgramParameter(p, parallel.COMPLETION_STATUS_KHR))) return setTimeout(linked, 50);
    const where = (p, name) => gl.getUniformLocation(p, name);
    u = {
      dy: where(strings, "u_dy"), glow: where(strings, "u_glow"), solid: where(strings, "u_solid"),
      lineColour: where(strings, "u_colour"), pulseColour: where(pulses, "u_colour"),
    };
    update();
  };
  linked();
}
