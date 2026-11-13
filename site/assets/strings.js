// The hero's strings, drawn in a worker on the canvas site.js hands over,
// so creating the WebGL context (190 ms on a first visit, as the GPU process
// starts; about 40 after) and every frame stay off the page's main thread.
// site.js sends the drawing's paths and colours with the canvas, then the
// pointer's place in the drawing's units, when to forget it, and when to
// stop.
const start = ({ canvas, paths, ink, accent }) => {
  const gl = canvas.getContext("webgl", { antialias: false, premultipliedAlpha: true, powerPreference: "low-power" });
  if (!gl) return () => {};
  // Each string as 40 segments, and an out string's straight tail as one
  // more; a vertex is x, y, how much a pluck moves it (the curve's middle
  // control points' weight, 3t(1-t)), and its string.
  const vertices = [];
  let inVertices = 0;
  const lines = paths.map((d, i) => {
    const [x0, y0, x1, y1, x2, y2, x3, y3, tail] = d.match(/-?[\d.]+/g).map(Number);
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
    if (i === 100) inVertices = vertices.length / 4;
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
  // The fade both programs share, the still image's own (share/strings.html): an
  // ellipse about (850, 300), 640 wide and 640 x 0.506 tall, full to 35% of
  // the way out, then to nothing, so a string glows only where it is drawn.
  const fade = "clamp((1.0 - length((v_pos - vec2(850.0, 300.0)) / vec2(640.0, 323.84))) / 0.65, 0.0, 1.0)";
  // The canvas comes at the still image's size, a pixel per cell of the
  // drawing, and is never resized. Every pixel is on or off: a brightness
  // is a share of pixels, those whose place in a 4 by 4 Bayer grid it
  // passes, the grid the still image was drawn on (rows counted from the
  // top).
  const cell = (1500 / canvas.width).toFixed(1), top = (canvas.height - 1).toFixed(1);
  const threshold = `
    float b2(vec2 p) { return 2.0 * p.x + 3.0 * p.y - 4.0 * p.x * p.y; }
    float threshold() {
      vec2 c = vec2(floor(gl_FragCoord.x), ${top} - floor(gl_FragCoord.y));
      return (4.0 * b2(mod(c, 2.0)) + b2(mod(floor(c / 2.0), 2.0)) + 0.5) / 16.0;
    }`;
  const strings = program(`
    attribute vec4 a;
    uniform vec4 u_dy[50];
    uniform vec4 u_glow[50];
    varying vec2 v_pos;
    varying float v_alpha;
    void main() {
      int i = int(a.w / 4.0);
      float k = mod(a.w, 4.0);
      vec4 m = vec4(k < 0.5, k >= 0.5 && k < 1.5, k >= 1.5 && k < 2.5, k >= 2.5);
      vec2 p = vec2(a.x, a.y + a.z * dot(u_dy[i], m));
      float glow = dot(u_glow[i], m);
      v_pos = p;
      v_alpha = min(1.0, glow * 3.0);
      // A string at rest is not drawn at all: all its points fall outside
      // the view, so the GPU fills no pixel for it.
      gl_Position = glow == 0.0 ? vec4(2.0, 2.0, 2.0, 1.0) : vec4(p.x / 750.0 - 1.0, 1.0 - p.y / 300.0, 0.0, 1.0);
    }`, `
    precision mediump float;
    uniform vec3 u_colour;
    varying vec2 v_pos;
    varying float v_alpha;
    ${threshold}
    void main() {
      // Nothing rings over the dot, which the still draws (11 units about
      // (900, 300), and a cell's margin).
      if (v_alpha * ${fade} <= threshold() || distance(v_pos, vec2(900.0, 300.0)) < 12.5) discard;
      gl_FragColor = vec4(u_colour, 1.0);
    }`);
  // A pulse is a point of light: x, y, brightness, size (in the drawing's
  // units).
  const pulses = program(`
    attribute vec4 a;
    varying vec2 v_pos;
    varying float v_alpha;
    void main() {
      v_pos = a.xy;
      v_alpha = a.z;
      gl_PointSize = max(1.0, a.w / ${cell});
      gl_Position = vec4(a.x / 750.0 - 1.0, 1.0 - a.y / 300.0, 0.0, 1.0);
    }`, `
    precision mediump float;
    uniform vec3 u_colour;
    varying vec2 v_pos;
    varying float v_alpha;
    ${threshold}
    void main() {
      vec2 c = gl_PointCoord * 2.0 - 1.0;
      if (v_alpha * 1.6 * exp(-3.0 * dot(c, c)) * ${fade} <= threshold()) discard;
      gl_FragColor = vec4(u_colour, 1.0);
    }`);
  const lineBuffer = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, lineBuffer);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array(vertices), gl.STATIC_DRAW);
  const pulseBuffer = gl.createBuffer();
  const pulseData = new Float32Array((48 * 10 + 1) * 4);
  gl.enableVertexAttribArray(0);
  // The shaders' inputs; asking for them waits for the programs to link,
  // which in a worker keeps no one waiting.
  const where = (p, name) => gl.getUniformLocation(p, name);
  const u = {
    dy: where(strings, "u_dy"), glow: where(strings, "u_glow"),
    lineColour: where(strings, "u_colour"), pulseColour: where(pulses, "u_colour"),
  };
  const dim = ink.map((c) => c * 0.6);
  const dys = new Float32Array(200);
  const glows = new Float32Array(200);
  // blank: the canvas holds nothing, so a frame with nothing to show is
  // skipped.
  let halo = 0, frame = 0, last = 0, blank = true;
  // The pointer: its latest place in the drawing's units, read once a frame,
  // and the place read the frame before.
  let pointerX = 0, pointerY = 0, pending = false, previousX = 0, previousY = 0, havePrevious = false;
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
    const x = pointerX, y = pointerY;
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
    const still = !ringing && !flight.length && halo <= 0.003;
    if (still && blank) {
      frame = last = 0;
      return;
    }
    blank = still;
    gl.clear(gl.COLOR_BUFFER_BIT);
    // The ringing strings.
    gl.useProgram(strings);
    gl.bindBuffer(gl.ARRAY_BUFFER, lineBuffer);
    gl.vertexAttribPointer(0, 4, gl.FLOAT, false, 0, 0);
    gl.uniform4fv(u.dy, dys);
    gl.uniform4fv(u.glow, glows);
    // The strings in run behind the headline, so they ring in a grey the
    // words stand clear of, as the still draws them; the strings out ring
    // in ink.
    gl.uniform3fv(u.lineColour, dim);
    gl.drawArrays(gl.LINES, 0, inVertices);
    gl.uniform3fv(u.lineColour, ink);
    gl.drawArrays(gl.LINES, inVertices, strandVertices - inVertices);
    gl.uniform3fv(u.lineColour, accent);
    gl.drawArrays(gl.LINES, strandVertices, vertices.length / 4 - strandVertices);
    // The requests, over the strings.
    gl.useProgram(pulses);
    gl.bindBuffer(gl.ARRAY_BUFFER, pulseBuffer);
    // Fresh storage each frame, so the GPU never makes the worker wait for
    // it to finish reading last frame's.
    gl.bufferData(gl.ARRAY_BUFFER, pulseData.subarray(0, n * 4), gl.STREAM_DRAW);
    gl.vertexAttribPointer(0, 4, gl.FLOAT, false, 0, 0);
    gl.uniform3fv(u.pulseColour, ink);
    gl.drawArrays(gl.POINTS, 0, accentAt);
    gl.uniform3fv(u.pulseColour, accent);
    gl.drawArrays(gl.POINTS, accentAt, n - accentAt);
    // Go on while anything still moves; otherwise the frame just drawn is
    // empty, and the loop stops until the pointer moves again.
    if (!still) frame = requestAnimationFrame(draw);
    else frame = last = 0;
  };
  // Motion stopped (out of view, a hidden tab, reduced motion): all still,
  // and one empty frame drawn as every frame is, so the last glow is not
  // left on screen.
  const silence = () => {
    cancelAnimationFrame(frame);
    last = 0;
    halo = 0;
    havePrevious = pending = false;
    flight.length = 0;
    for (const line of lines) line.amp = 0;
    frame = blank ? 0 : requestAnimationFrame(draw);
  };
  // A ringing string draws one cell wide.
  return (data) => {
    if (data.forget) havePrevious = pending = false;
    if (data.stop) silence();
    if (data.at) {
      [pointerX, pointerY] = data.at;
      pending = true;
      if (!frame) frame = requestAnimationFrame(draw);
    }
  };
};
// The first message starts the strings; the rest go to them. A context the
// GPU lost (a reset, a switch between GPUs) is given back, and the strings
// start again on it; a start that fails leaves the drawing still.
let handle = null;
onmessage = ({ data }) => {
  if (handle) return handle(data);
  const begin = () => {
    try {
      handle = start(data);
    } catch (error) {
      handle = () => {};
      throw error;
    }
  };
  data.canvas.addEventListener("webglcontextlost", (event) => event.preventDefault());
  data.canvas.addEventListener("webglcontextrestored", begin);
  begin();
};
