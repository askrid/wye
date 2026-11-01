// Copy buttons for command and config blocks. Every page works without
// this script: the text stays selectable (site/DESIGN.md, "Never the slow
// part").
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
