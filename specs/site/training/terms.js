// Glossary terms in the training pages: the first mention of each term in a
// section links to its glossary entry, and hovering it (or tapping it, on a
// touch screen) shows the definition. build.mjs embeds the glossary in the
// page as JSON (#mt-glossary), from training/glossary.md.

(() => {
  const data = document.getElementById("mt-glossary");
  const main = document.querySelector("main");
  if (!data || !main) return;
  const { href, terms } = JSON.parse(data.textContent);
  if (!terms.length) return;
  const onGlossary = location.pathname.endsWith(href.split("/").pop());

  // Every form of every term, longest first, so "maintenance margin" wins
  // over "margin".
  const forms = new Map();
  for (const t of terms) for (const f of [t.term, ...t.aliases]) forms.set(f.toLowerCase(), t);
  const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const alternatives = [...forms.keys()].sort((a, b) => b.length - a.length).map(escape);
  const pattern = new RegExp(`(?<![\\w-])(${alternatives.join("|")})(?![\\w-])`, "gi");
  const SKIP = "a, code, pre, h1, h2, h3, h4, h5, h6, summary, th, script, style, .mt-lab, .callout-title, .term";

  /** Whether the match at [start, end) is part of a phrase the term excludes. */
  function excluded(term, text, start, end) {
    const lower = text.toLowerCase();
    for (const phrase of term.not) {
      const p = phrase.toLowerCase();
      for (let i = lower.indexOf(p); i !== -1; i = lower.indexOf(p, i + 1))
        if (i <= start && i + p.length >= end) return true;
    }
    return false;
  }

  function mark(block, seen, section) {
    const walker = document.createTreeWalker(block, NodeFilter.SHOW_TEXT, {
      acceptNode: (n) => (n.parentElement.closest(SKIP) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT),
    });
    const nodes = [];
    while (walker.nextNode()) nodes.push(walker.currentNode);
    for (const node of nodes) {
      const text = node.nodeValue;
      const parts = [];
      let last = 0;
      for (const m of text.matchAll(pattern)) {
        const term = forms.get(m[0].toLowerCase());
        const end = m.index + m[0].length;
        // Once per section, and never in the term's own glossary entry.
        if (seen.has(term.id) || term.id === section || excluded(term, text, m.index, end)) continue;
        seen.add(term.id);
        parts.push(text.slice(last, m.index));
        const a = document.createElement("a");
        a.className = "term";
        a.href = onGlossary ? `#${term.id}` : `${href}#${term.id}`;
        a.dataset.term = term.id;
        a.textContent = m[0];
        parts.push(a);
        last = end;
      }
      if (!parts.length) continue;
      parts.push(text.slice(last));
      node.replaceWith(...parts);
    }
  }

  // A term links once per section: an h2 in a guide, an entry (h3) in the
  // glossary. The headings are children of <main>.
  let seen = new Set();
  let section = null;
  for (const child of main.children) {
    if (/^H[23]$/.test(child.tagName)) {
      if (child.tagName === "H2" || onGlossary) seen = new Set();
      section = child.id;
      continue;
    }
    mark(child, seen, section);
  }

  // The popover: one for the page, filled for whichever term is hovered.
  const byId = new Map(terms.map((t) => [t.id, t]));
  const pop = document.createElement("div");
  pop.className = "term-pop";
  pop.setAttribute("role", "tooltip");
  pop.hidden = true;
  document.body.appendChild(pop);
  let current = null;
  let hideTimer = null;

  function show(a) {
    clearTimeout(hideTimer);
    const term = byId.get(a.dataset.term);
    current = a;
    pop.replaceChildren();
    const title = document.createElement("p");
    title.className = "term-pop-title";
    title.textContent = term.term;
    const body = document.createElement("p");
    body.innerHTML = term.html; // Rendered by the build from the glossary.
    const more = document.createElement("a");
    more.className = "term-pop-more";
    more.href = a.href;
    more.textContent = onGlossary ? "Go to the entry →" : "More in the glossary →";
    pop.append(title, body, more);
    pop.hidden = false;
    const r = a.getBoundingClientRect();
    const width = Math.min(340, innerWidth - 24);
    pop.style.width = `${width}px`;
    const left = Math.min(Math.max(12, r.left), innerWidth - width - 12);
    const below = r.bottom + 8 + pop.offsetHeight < innerHeight;
    pop.style.left = `${left + scrollX}px`;
    pop.style.top = `${(below ? r.bottom + 6 : r.top - pop.offsetHeight - 6) + scrollY}px`;
  }
  function hideSoon() {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      pop.hidden = true;
      current = null;
    }, 150);
  }

  let touch = false;
  main.addEventListener("pointerdown", (e) => (touch = e.pointerType === "touch"));
  for (const a of main.querySelectorAll("a.term")) {
    a.addEventListener("pointerenter", (e) => e.pointerType !== "touch" && show(a));
    a.addEventListener("pointerleave", hideSoon);
    a.addEventListener("focus", () => show(a));
    a.addEventListener("blur", hideSoon);
    // On a touch screen, the first tap shows the definition, the second follows the link.
    a.addEventListener("click", (e) => {
      if (touch && (current !== a || pop.hidden)) {
        e.preventDefault();
        show(a);
      }
    });
  }
  pop.addEventListener("pointerenter", () => clearTimeout(hideTimer));
  pop.addEventListener("pointerleave", hideSoon);
  document.addEventListener("keydown", (e) => e.key === "Escape" && (pop.hidden = true));
  document.addEventListener("pointerdown", (e) => {
    if (!pop.hidden && !pop.contains(e.target) && !e.target.closest?.("a.term")) pop.hidden = true;
  });
})();
