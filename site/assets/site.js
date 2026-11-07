// Copy buttons on command and config blocks, a convenience the page works
// without: with scripts off the text stays selectable.
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
      await navigator.clipboard.writeText(pre.innerText);
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

// Ambient motion (site.css, "Breathing"): the M key plays or pauses it,
// whatever the system's reduced-motion setting, remembered in this browser.
// The key, not the character: in a Korean layout it types "ㅡ". On
// localhost a button does the same, for checking the page while editing it.
const root = document.documentElement;
try {
  const saved = localStorage.getItem("wye-motion");
  if (saved === "on" || saved === "off") root.dataset.motion = saved;
} catch {}
const moving = () =>
  root.dataset.motion
    ? root.dataset.motion === "on"
    : !matchMedia("(prefers-reduced-motion: reduce)").matches;
const toggleMotion = () => {
  root.dataset.motion = moving() ? "off" : "on";
  try {
    localStorage.setItem("wye-motion", root.dataset.motion);
  } catch {}
};
addEventListener("keydown", (event) => {
  if (event.code !== "KeyM" || event.metaKey || event.ctrlKey || event.altKey || event.isComposing) return;
  if (event.target instanceof Element && event.target.closest("input, textarea, [contenteditable]")) return;
  toggleMotion();
});
if (["localhost", "127.0.0.1", "[::1]"].includes(location.hostname)) {
  const play = document.createElement("button");
  play.type = "button";
  play.className = "copy";
  play.style.cssText = "position: fixed; right: 1rem; bottom: 1rem; z-index: 1;";
  const label = () => (play.textContent = moving() ? "[pause]" : "[play]");
  play.addEventListener("click", () => {
    toggleMotion();
    label();
  });
  addEventListener("keydown", (event) => event.code === "KeyM" && queueMicrotask(label));
  label();
  document.body.append(play);
}
