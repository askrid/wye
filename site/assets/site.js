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

// The headline's lines as strings, and requests along them. Pulses of
// light enter on the incoming strings, speed into the dot, and leave on the
// string that line routes to (one on the orange thread, the failover); a
// string the pointer crosses is plucked, rings, and sends a pulse of its own.
// It all lives on the GPU: the strings' shapes are uploaded once, and a frame
// sends each string's displacement and each pulse's place, then draws twice.
// It runs only while the hero is in view and the tab shown, on the
// low-power GPU, and never under prefers-reduced-motion; without WebGL the
// drawing just sways (CSS).
const period = document.querySelector(".period");
const canvas = document.createElement("canvas");
const gl = period && canvas.getContext("webgl", { antialias: true, premultipliedAlpha: true, powerPreference: "low-power" });
if (gl) {
  const reduce = matchMedia("(prefers-reduced-motion: reduce)");
  canvas.setAttribute("aria-hidden", "true");
  period.append(canvas);
  const paths = [...period.querySelectorAll(".strands path"), period.querySelector(".thread")];
  // Each string as 40 segments; a vertex is x, y, how much a pluck moves it
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
    return { at, amp: 0, since: 0, dy: 0 };
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
      gl_Position = vec4(p.x / 750.0 - 1.0, 1.0 - p.y / 300.0, 0.0, 1.0);
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
    uniform float u_density;
    varying vec2 v_pos;
    varying float v_alpha;
    void main() {
      v_pos = a.xy;
      v_alpha = a.z;
      gl_PointSize = a.w * u_density;
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
  const pulseData = new Float32Array((96 * 10 + 1) * 4);
  gl.bindBuffer(gl.ARRAY_BUFFER, pulseBuffer);
  gl.bufferData(gl.ARRAY_BUFFER, pulseData.byteLength, gl.DYNAMIC_DRAW);
  gl.enableVertexAttribArray(0);
  gl.enable(gl.BLEND);
  const where = (p, name) => gl.getUniformLocation(p, name);
  const u = {
    dy: where(strings, "u_dy"), glow: where(strings, "u_glow"), solid: where(strings, "u_solid"),
    lineColour: where(strings, "u_colour"), density: where(pulses, "u_density"), pulseColour: where(pulses, "u_colour"),
  };
  const dys = new Float32Array(200);
  const glows = new Float32Array(200);
  const rgb = (name) => {
    const hex = getComputedStyle(period).getPropertyValue(name).trim().slice(1);
    return [0, 2, 4].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
  };
  let ink, accent, density = 1, halo = 0, running = false, visible = false, lost = false, frame = 0, last = 0, spawn = 0, previous = null;
  // One canvas pixel per CSS pixel: a ringing string draws a pixel wide, as
  // the drawing's 0.9-unit strokes do, and the pulses are soft anyway.
  const fit = () => {
    const box = canvas.getBoundingClientRect();
    canvas.width = Math.round(box.width * density);
    canvas.height = Math.round(box.height * density);
    gl.viewport(0, 0, canvas.width, canvas.height);
    ink = rgb("--ink");
    accent = rgb("--accent");
  };
  // Where a string is at fraction t of its length, displaced as it rings.
  const along = (line, t) => {
    const at = line.at, f = Math.min(Math.max(t, 0), 1) * (at.length - 1), k = Math.min(Math.floor(f), at.length - 2), r = f - k;
    const [ax, ay, aw] = at[k], [bx, by, bw] = at[k + 1];
    return [ax + (bx - ax) * r, ay + (by - ay) * r + (aw + (bw - aw) * r) * line.dy];
  };
  // Pulses: a string in, then its string out. In, they gather speed into the
  // dot; out, they ease off.
  const flight = [];
  const launch = (i, out, t, bright) => {
    if (flight.length < 96) flight.push({ i, out, t, bright });
  };
  const IN = 1500, OUT = 1900;
  const draw = (now) => {
    frame = requestAnimationFrame(draw);
    const dt = Math.min(now - (last || now), 50);
    last = now;
    // A new request every 120 ms or so; one on the failover every 4 s.
    spawn -= dt;
    if (spawn <= 0) {
      spawn = 60 + Math.random() * 80;
      launch(Math.floor(Math.random() * 100), false, 0, 0.55 + Math.random() * 0.3);
    }
    if (now % 4000 < dt) launch(59, false, 0, 0.9);
    // Strings: a plucked one rings at 3 Hz, its swing dying over half a
    // second; its copy glows in over 60 ms and out over 700, to nothing.
    lines.forEach((line, i) => {
      const age = now - line.since;
      dys[i] = glows[i] = line.dy = 0;
      if (!line.amp || age < 0) return;
      const glow = 0.55 * (1 - Math.exp(-Math.abs(line.amp) / 3)) * (1 - Math.exp(-age / 60)) * Math.exp(-age / 700);
      if (age > 300 && glow < 0.002) {
        line.amp = 0;
        return;
      }
      dys[i] = line.dy = line.amp * Math.exp(-age / 520) * Math.sin((2 * Math.PI * 3 * age) / 1000);
      glows[i] = glow;
    });
    // Pulses: a head and nine fading points close behind it, a streak.
    let n = 0, accentAt = 0;
    const write = (thread) => {
      for (const p of flight) {
        const line = p.out ? lines[route(p.i)] : lines[p.i];
        if ((line === lines[thread]) !== thread) continue;
        const ease = p.out ? 1 - (1 - p.t) * (1 - p.t) : p.t * p.t;
        for (let k = 0; k < 10; k++) {
          const [x, y] = along(line, ease - k * 0.006);
          pulseData.set([x, y, p.bright * (1 - k / 10) ** 2 * 0.8, 10 - k * 0.7], n * 4);
          n++;
        }
      }
    };
    for (const p of flight) p.t += dt / (p.out ? OUT : IN);
    // The dot carries the traffic: each request through it brightens a soft
    // halo, which dies away over 400 ms.
    halo *= Math.exp(-dt / 400);
    for (let k = flight.length - 1; k >= 0; k--) {
      const p = flight[k];
      if (p.t >= 1 && !p.out) {
        p.out = true;
        p.t = 0;
        halo = Math.min(1, halo + 0.18 * p.bright);
      } else if (p.t >= 1) flight.splice(k, 1);
    }
    pulseData.set([900, 300, halo * 0.35, 70], 0);
    n = 1;
    write(false);
    accentAt = n;
    write(true);
    gl.clearColor(0, 0, 0, 0);
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
    // The pulses, added light on light.
    gl.useProgram(pulses);
    gl.blendFunc(gl.ONE, gl.ONE);
    gl.bindBuffer(gl.ARRAY_BUFFER, pulseBuffer);
    gl.bufferSubData(gl.ARRAY_BUFFER, 0, pulseData.subarray(0, n * 4));
    gl.vertexAttribPointer(0, 4, gl.FLOAT, false, 0, 0);
    gl.uniform1f(u.density, density);
    gl.uniform3fv(u.pulseColour, ink);
    gl.drawArrays(gl.POINTS, 0, accentAt);
    gl.uniform3fv(u.pulseColour, accent);
    gl.drawArrays(gl.POINTS, accentAt, n - accentAt);
  };
  // Where the pointer crosses a string at rest, by the string's own points.
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
      const amp = Math.min(5, Math.max(1.5, Math.abs(point.y - previous.y) * 0.4)) * Math.sign(point.y - previous.y || 1);
      const now = performance.now();
      // A plucked string's neighbours pick up a faint ring, 40 ms later per
      // string away, unless they already ring louder.
      const ring = (j, share, delay) => {
        const line = lines[j];
        if (!line || j === thread || Math.abs(line.amp) > Math.abs(amp * share)) return;
        line.amp = amp * share;
        line.since = now + delay;
      };
      lines.forEach((line, i) => {
        const before = yAt(line, previous.x), after = yAt(line, point.x);
        if (before === null || after === null) return;
        if (Math.sign(previous.y - before) !== Math.sign(point.y - after)) {
          line.amp = amp;
          line.since = now;
          for (const d of [1, 2]) {
            ring(i - d, d === 1 ? 0.35 : 0.12, 40 * d);
            ring(i + d, d === 1 ? 0.35 : 0.12, 40 * d);
          }
          // And sends a request on from where it was plucked.
          const t = (point.x - line.at[0][0]) / (line.at[line.at.length - 1][0] - line.at[0][0]);
          if (i < 100) launch(i, false, Math.sqrt(Math.max(t, 0)), 1);
          else if (i === thread) launch(59, true, 1 - Math.sqrt(1 - Math.min(t, 1)), 1);
          else launch(i - 100 + (i - 100 >= 59 ? 1 : 0), true, 1 - Math.sqrt(1 - Math.min(t, 1)), 1);
        }
      });
    }
    previous = point;
  };
  // A pointer that left the window, or a page that scrolled under it, starts
  // afresh, so its next move does not pluck every string in between.
  const forget = () => {
    previous = null;
  };
  const update = () => {
    const on = visible && !lost && !document.hidden && !reduce.matches;
    if (on === running) return;
    running = on;
    if (on) {
      fit();
      last = 0;
      frame = requestAnimationFrame(draw);
      addEventListener("pointermove", pluck, { passive: true });
      addEventListener("scroll", forget, { passive: true });
      document.documentElement.addEventListener("pointerleave", forget);
    } else {
      cancelAnimationFrame(frame);
      removeEventListener("pointermove", pluck);
      removeEventListener("scroll", forget);
      document.documentElement.removeEventListener("pointerleave", forget);
      previous = null;
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
  update();
}
