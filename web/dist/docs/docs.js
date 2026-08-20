
const search = document.getElementById("search");
const results = document.getElementById("results");
const list = document.getElementById("results-list");
let index = null;
async function ensureIndex() {
  if (!index) index = await (await fetch("/docs/search-index.json")).json();
  return index;
}
search?.addEventListener("input", async () => {
  const q = search.value.trim().toLowerCase();
  if (q.length < 2) { results.classList.add("hidden"); return; }
  const idx = await ensureIndex();
  const hits = idx.filter((p) => p.title.toLowerCase().includes(q) || p.text.includes(q)).slice(0, 8);
  list.innerHTML = hits.length
    ? hits.map((h) => `<a class="block py-1.5 text-sm text-fade hover:text-accent" href="${h.url}"><span class="text-ink">${h.title}</span> · ${h.section}</a>`).join("")
    : '<div class="py-1.5 text-sm text-fade">no matches</div>';
  results.classList.remove("hidden");
});
document.addEventListener("click", (e) => {
  if (!results.contains(e.target) && e.target !== search) results.classList.add("hidden");
});
document.addEventListener("keydown", (e) => {
  if (e.key === "/" && document.activeElement !== search) { e.preventDefault(); search?.focus(); }
  if (e.key === "Escape") results.classList.add("hidden");
});
document.getElementById("menu")?.addEventListener("click", () => {
  document.getElementById("sidebar").classList.toggle("hidden");
});
for (const btn of document.querySelectorAll(".codeblock .copy")) {
  btn.addEventListener("click", () => {
    navigator.clipboard.writeText(btn.nextElementSibling.textContent).then(() => {
      btn.textContent = "✓"; setTimeout(() => { btn.textContent = "copy"; }, 1200);
    });
  });
}
