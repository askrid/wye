// Conveniences the page works without, with scripts off as well: copy
// buttons on command and config blocks, and a play button for the junction
// where the reader asked for reduced motion.

// Copy buttons: without them the text stays selectable.
for (const pre of document.querySelectorAll('pre.code[data-lang="bash"], pre.code[data-lang="toml"]')) {
  const wrap = document.createElement("div");
  wrap.className = "code-wrap";
  pre.before(wrap);
  wrap.append(pre);
  const button = document.createElement("button");
  button.type = "button";
  button.className = "copy";
  button.textContent = "[copy]";
  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(pre.innerText);
      button.textContent = "[copied]";
    } catch {
      // No clipboard access: select the text instead.
      getSelection().selectAllChildren(pre);
      button.textContent = "[selected]";
    }
    setTimeout(() => {
      button.textContent = "[copy]";
    }, 1500);
  });
  wrap.append(button);
}

// The junction plays its sequence on its own unless the reader asked for
// reduced motion; then it shows the end state, and this button plays the
// sequence once, on request.
const junction = document.querySelector(".junction");
if (junction && matchMedia("(prefers-reduced-motion: reduce)").matches) {
  const play = document.createElement("button");
  play.type = "button";
  play.className = "play";
  play.textContent = "[play]";
  play.addEventListener("click", () => {
    junction.classList.add("playing");
    play.disabled = true;
    play.textContent = "[playing]";
    // The run ends 8.05 s in: the 7 s sequence plus the last event's 1.05 s
    // delay (site.css).
    setTimeout(() => {
      junction.classList.remove("playing");
      play.disabled = false;
      play.textContent = "[replay]";
    }, 8200);
  });
  junction.querySelector("figcaption").before(play);
}
