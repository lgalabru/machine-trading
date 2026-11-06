// Renders specs/ as the Machine Trading Protocol site, into site/_site.
//
// The index lists every spec as a tree, the way paymentauth.org lists MPP's.
// Every other Markdown file becomes a page at the same path, with numbered
// sections and a table of contents; links between specs are rewritten to the
// pages. Links are relative, so the site works under any base path, such as
// a project's GitHub Pages URL.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Marked } from "marked";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..");
const out = path.join(here, "_site");
const NAME = "Machine Trading Protocol";
// Where the site is served: link previews need absolute URLs.
const SITE = "https://lgalabru.github.io/machine-trading";
const SOURCE = "https://github.com/lgalabru/machine-trading/tree/main/specs";
// Folders of the index after the core spec, in order.
const FOLDERS = [
  { dir: "instruments", title: "Instruments" },
  { dir: "venues", title: "Venues" },
  { dir: "examples", title: "Examples" },
  // Guides for people, not readers implementing the specs.
  { dir: "training", title: "Training" },
  { dir: "training/instruments", title: "Training: instruments" },
];
// Training pages load the labs' runtime, and their own models: the .js file
// next to the guide, such as training/instruments/perp.js.
const isTraining = (page) => page.startsWith("training/");
const folderId = (dir) => dir.replace(/\//g, "-");

function walk(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) return entry.name === "site" ? [] : walk(full);
    return [full];
  });
}

const rel = (file) => path.relative(root, file).split(path.sep).join("/");
const pageOf = (file) => rel(file).replace(/\.md$/, ".html");
const titleOf = (markdown) => (markdown.match(/^#\s+(.+)$/m)?.[1] ?? "").replace(/`/g, "").trim();
const statusOf = (markdown) => markdown.match(/^Status:\s*(.+)$/m)?.[1]?.trim();
const slug = (text) =>
  text
    .toLowerCase()
    .replace(/<[^>]+>/g, "")
    .replace(/[^\p{L}\p{N}\s-]/gu, "")
    .trim()
    .replace(/\s+/g, "-");
const escape = (text) =>
  text.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

/** A relative link from one page to another, both relative to the site root. */
function link(from, to) {
  const target = path.posix.relative(path.posix.dirname(from), to);
  return target === "" ? path.posix.basename(to) : target;
}

/**
 * Rewrite a link written in a spec to the site: a .md file to its page, and
 * a directory to its folder in the index.
 */
function rewrite(href, page) {
  if (/^([a-z]+:|#|\/)/i.test(href)) return href;
  const [target, anchor] = href.split("#");
  const resolved = path.posix.normalize(path.posix.join(path.posix.dirname(page), target));
  if (target.endsWith("/")) return `${link(page, "index.html")}#${path.posix.basename(resolved)}`;
  const to = resolved === "README.md" ? "index.html" : resolved.replace(/\.md$/, ".html");
  return link(page, to) + (anchor ? `#${anchor}` : "");
}

// Callouts, in guides: a block between `:::kind Optional title` and `:::`.
const CALLOUTS = {
  glance: ["◎", "At a glance"],
  eli5: ["✦", "Explain it like I'm five"],
  use: ["✓", "Use it when"],
  avoid: ["✕", "Don't use it when"],
  risk: ["!", "Risk"],
  note: ["i", "Note"],
  example: ["→", "Example"],
  history: ["§", "A bit of history"],
  check: ["?", "Check yourself"],
};
const callout = {
  name: "callout",
  level: "block",
  start: (src) => src.match(/^:::/m)?.index,
  tokenizer(src) {
    const match = /^:::([a-z0-9-]+)[ \t]*([^\n]*)\n([\s\S]*?)\n:::[ \t]*(?:\n+|$)/.exec(src);
    if (!match) return undefined;
    const [raw, kind, title, inner] = match;
    const token = { type: "callout", raw, kind, title: title.trim(), tokens: [] };
    if (kind === "group") token.tokens = this.lexer.blockTokens(inner.replace(/^::/gm, ":::"), []);
    else this.lexer.blockTokens(inner, token.tokens);
    return token;
  },
  renderer(token) {
    const body = this.parser.parse(token.tokens);
    // ::group wraps callouts side by side; inside it they open with `::`.
    if (token.kind === "group") return `<div class="callouts">${body}</div>\n`;
    const known = CALLOUTS[token.kind];
    if (!known) throw new Error(`unknown callout :::${token.kind}`);
    const [icon, fallback] = known;
    const title = token.title ? new Marked().parseInline(token.title) : fallback;
    if (token.kind === "check")
      return `<details class="callout callout-check"><summary>${title}</summary>${body}</details>\n`;
    return `<aside class="callout callout-${token.kind}"><p class="callout-title"><span class="icon" aria-hidden="true">${icon}</span>${title}</p>${body}</aside>\n`;
  },
};

/**
 * A lab, in guides: a ```lab block of JSON naming one of the page's models,
 * its controls and presets. Checked here, so a broken lab fails the build
 * rather than the page.
 */
function lab(json, page, models) {
  let config;
  try {
    config = JSON.parse(json);
  } catch (error) {
    throw new Error(`${page}: a lab is not valid JSON: ${error.message}`);
  }
  const fail = (why) => {
    throw new Error(`${page}: lab ${config.model ?? "?"}: ${why}`);
  };
  if (!models.includes(config.model)) fail(`no model registers it in ${page.replace(/\.html$/, ".js")}`);
  if (!Array.isArray(config.controls)) fail("controls must be an array");
  const ids = new Set();
  for (const c of config.controls) {
    if (!c.id || !c.label || c.value === undefined) fail("every control needs an id, a label and a value");
    if (ids.has(c.id)) fail(`two controls are called ${c.id}`);
    ids.add(c.id);
    if (c.type === "choice" && !c.options?.some(([v]) => v === c.value)) fail(`${c.id}: its value is not an option`);
    if (c.type !== "choice" && !(c.min <= c.value && c.value <= c.max)) fail(`${c.id}: its value is outside min..max`);
  }
  for (const p of config.presets ?? [])
    for (const id of Object.keys(p.set ?? {})) if (!ids.has(id)) fail(`preset ${p.label} sets unknown control ${id}`);
  const inline = (text) => (text ? new Marked().parseInline(text) : "");
  const data = JSON.stringify(config).replace(/</g, "\\u003c");
  return `<figure class="mt-lab">
${config.title ? `<p class="mt-lab-title">${inline(config.title)}</p>` : ""}${config.lede ? `<p class="mt-lab-lede">${inline(config.lede)}</p>` : ""}
<div class="mt-lab-body"><noscript><p>This figure needs JavaScript.</p></noscript></div>
${config.caption ? `<figcaption>${new Marked().parse(config.caption)}</figcaption>` : ""}
<script type="application/json">${data}</script>
</figure>
`;
}

/**
 * The glossary, training/glossary.md: every `### Term` is an entry, with
 * optional `Also:` (other forms that refer to it) and `Not:` (phrases that
 * contain it but mean something else) lines, which the page doesn't show.
 * The entry's first paragraph is its definition on hover, in every guide.
 */
const GLOSSARY = "training/glossary.md";
function glossaryOf(markdown) {
  const list = (line) => (line ?? "").split(",").map((s) => s.trim()).filter(Boolean);
  return markdown
    .split(/^### /m)
    .slice(1)
    .map((chunk) => {
      const [heading, ...rest] = chunk.split("\n");
      const term = heading.trim();
      const body = rest.join("\n");
      const also = body.match(/^Also:(.*)$/m)?.[1];
      const not = body.match(/^Not:(.*)$/m)?.[1];
      const text = body.replace(/^(Also|Not):.*$/gm, "").split(/^## /m)[0].trim();
      const first = text.split(/\n\s*\n/)[0].replace(/\n/g, " ");
      const html = new Marked().parseInline(first).replace(/<\/?a\b[^>]*>/g, "");
      return { term, id: slug(term), aliases: list(also), not: list(not), html };
    });
}
const stripGlossaryData = (markdown) => markdown.replace(/^(Also|Not):.*\n/gm, "");

/** The models a training page's script registers. */
function modelsOf(file) {
  const script = file.replace(/\.md$/, ".js");
  if (!fs.existsSync(script)) return [];
  return [...fs.readFileSync(script, "utf8").matchAll(/MT\.register\(\s*["']([^"']+)["']/g)].map((m) => m[1]);
}

/** A spec's body as HTML, with its sections numbered, and its table of contents. */
function render(markdown, page, models = []) {
  const toc = [];
  let section = 0;
  let subsection = 0;
  const marked = new Marked({
    gfm: true,
    extensions: isTraining(page) ? [callout] : [],
    renderer: {
      code({ text, lang }) {
        if (lang === "lab" && isTraining(page)) return lab(text, page, models);
        return false;
      },
      heading({ tokens, depth }) {
        const html = this.parser.parseInline(tokens);
        if (depth === 1) return "";
        const id = slug(html);
        let number = "";
        // The abstract isn't numbered, as in an RFC.
        if (depth <= 3 && !/^abstract$/i.test(html)) {
          if (depth === 2) {
            section += 1;
            subsection = 0;
            number = `${section}.`;
          } else {
            subsection += 1;
            number = `${section}.${subsection}.`;
          }
        }
        if (depth <= 3) toc.push({ depth, id, number, html });
        const label = number ? `<a class="number" href="#${id}">${number}</a> ` : "";
        return `<h${depth} id="${id}">${label}${html}</h${depth}>\n`;
      },
      link({ href, title, tokens }) {
        const text = this.parser.parseInline(tokens);
        return `<a href="${escape(rewrite(href, page))}"${title ? ` title="${escape(title)}"` : ""}>${text}</a>`;
      },
    },
  });
  // The title and status are rendered in the document header instead.
  const body = marked.parse(markdown.replace(/^Status:.*$/m, ""));
  return { body, toc };
}

function tocHtml(toc) {
  if (toc.length < 3) return "";
  const items = toc
    .map((t) => {
      const number = t.number ? `<span class="number">${t.number}</span> ` : "";
      return `<li class="depth-${t.depth}"><a href="#${t.id}">${number}${t.html.replace(/<\/?a[^>]*>/g, "")}</a></li>`;
    })
    .join("");
  return `<nav class="toc"><h2 id="toc">Table of Contents</h2><ul>${items}</ul></nav>`;
}

let glossary = [];
const glossaryData = (page) =>
  `<script type="application/json" id="mt-glossary">${JSON.stringify({
    href: link(page, GLOSSARY.replace(/\.md$/, ".html")),
    terms: glossary,
  }).replace(/</g, "\\u003c")}</script>`;

// Highlights the table of contents' entry for the section being read.
const TOC_SPY = `<script>
(() => {
  const links = [...document.querySelectorAll(".toc a[href^='#']")];
  const sections = links.map((a) => document.getElementById(decodeURIComponent(a.hash.slice(1)))).filter(Boolean);
  if (!sections.length) return;
  const update = () => {
    let current = sections[0];
    for (const s of sections) if (s.getBoundingClientRect().top < 120) current = s;
    for (const a of links) a.classList.toggle("active", a.hash.slice(1) === current.id);
  };
  addEventListener("scroll", update, { passive: true });
  update();
})();
</script>`;

function layout({ page, title, head, body }) {
  const index = page === "index.html";
  const trail = [`<a href="${link(page, "index.html")}">${NAME}</a>`]
    .concat(
      page
        .split("/")
        .slice(0, -1)
        .map((dir, i, dirs) => `<a href="${link(page, "index.html")}#${folderId(dirs.slice(0, i + 1).join("/"))}">${dir}</a>`),
    )
    .join('<span class="sep">/</span>');
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escape(index ? `${NAME} Specifications` : `${title} · ${NAME}`)}</title>
<meta name="description" content="${NAME}: what an agent may trade on someone else's behalf, and the limits it must stay within.">
<meta property="og:type" content="website">
<meta property="og:site_name" content="Machine Trading">
<meta property="og:title" content="${escape(index ? `${NAME} Specifications` : `${title} · ${NAME}`)}">
<meta property="og:description" content="What an agent may trade on someone else's behalf, and the limits it must stay within.">
<meta property="og:url" content="${SITE}/${index ? "" : page}">
<meta property="og:image" content="${SITE}/social.jpg">
<meta property="og:image:width" content="1280">
<meta property="og:image:height" content="640">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:image" content="${SITE}/social.jpg">
<link rel="stylesheet" href="${link(page, "style.css")}">
${
  isTraining(page)
    ? `<link rel="stylesheet" href="${link(page, "training/mt.css")}">
<script defer src="${link(page, "training/mt.js")}"></script>
${fs.existsSync(path.join(root, page.replace(/\.html$/, ".js"))) ? `<script defer src="${path.posix.basename(page).replace(/\.html$/, ".js")}"></script>` : ""}
<script defer src="${link(page, "training/terms.js")}"></script>`
    : ""
}
</head>
<body class="${index ? "index" : isTraining(page) ? "spec training" : "spec"}">
${index ? "" : `<p class="trail">${trail}</p>`}
${head}
${body}
${isTraining(page) ? glossaryData(page) : ""}
${index ? "" : TOC_SPY}
</body>
</html>
`;
}

function write(page, html) {
  const file = path.join(out, page);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, html);
}

fs.rmSync(out, { recursive: true, force: true });
const files = walk(root);
const specs = files
  .filter((f) => f.endsWith(".md") && rel(f) !== "README.md")
  .map((file) => {
    const markdown = fs.readFileSync(file, "utf8");
    return { file, page: pageOf(file), title: titleOf(markdown), status: statusOf(markdown), markdown };
  });

const glossarySpec = specs.find((s) => rel(s.file) === GLOSSARY);
if (glossarySpec) {
  glossary = glossaryOf(glossarySpec.markdown);
  glossarySpec.markdown = stripGlossaryData(glossarySpec.markdown);
}
for (const spec of specs) {
  const { body, toc } = render(spec.markdown, spec.page, modelsOf(spec.file));
  const meta = spec.status
    ? `<dl class="meta"><dt>Status</dt><dd>${escape(spec.status)}</dd><dt>Source</dt><dd><a href="${SOURCE}">${SOURCE.replace("https://", "")}</a></dd></dl>`
    : "";
  write(
    spec.page,
    layout({ ...spec, head: `<h1>${escape(spec.title)}</h1>${meta}${tocHtml(toc)}`, body: `<main>${body}</main>` }),
  );
}

// The index: the README's opening paragraphs, then the tree of specs.
const readme = fs.readFileSync(path.join(root, "README.md"), "utf8");
const intro = new Marked({ gfm: true }).parse(readme.replace(/^#\s+.*$/m, "").split(/^##\s/m)[0].trim());
const core = specs.find((s) => s.page === "mandate.html");
const entry = (child, last) =>
  `<div class="spec-entry"><span class="branch">${last ? "└──" : "├──"}</span> <a class="spec-title" href="${escape(child.href)}">${escape(child.title)}</a> <span class="formats">${child.note}</span></div>`;
let tree = `<div class="folder"><a class="spec-title" href="${core.page}"><span class="folder-icon">~/</span>${escape(core.title)}</a> <span class="formats">${escape(core.status ?? "")}</span></div>`;
for (const { dir, title } of FOLDERS) {
  const children = files
    .filter((f) => path.posix.dirname(rel(f)) === dir && /\.(md|json)$/.test(f))
    .sort()
    .map((f) => {
      const spec = specs.find((s) => s.file === f);
      if (!spec) return { href: rel(f), title: path.basename(f), note: "JSON" };
      const short = spec.title.replace(/^.*?:\s*/, "").replace(/,\s*v\d+$/, "");
      const name = short.charAt(0).toUpperCase() + short.slice(1);
      return { href: spec.page, title: name, note: escape(spec.status ?? "") };
    });
  tree += `<div class="folder" id="${folderId(dir)}"><span class="folder-icon">~/</span>${title}</div>`;
  tree += children.map((c, i) => entry(c, i === children.length - 1)).join("");
}
write(
  "index.html",
  layout({
    page: "index.html",
    title: NAME,
    head: `<h1>${NAME} Specifications</h1><p class="header-links"><a href="${SOURCE}">Source</a><span class="sep">·</span><a href="mandate.html">Mandate v0</a><span class="sep">·</span><a href="bench/">Benchmark</a></p>`,
    body: `<div class="intro">${intro}</div><div class="tree">${tree}</div>`,
  }),
);

for (const file of files.filter((f) => f.endsWith(".json"))) {
  fs.mkdirSync(path.dirname(path.join(out, rel(file))), { recursive: true });
  fs.copyFileSync(file, path.join(out, rel(file)));
}
// Training pages' models, next to their guides.
for (const file of files.filter((f) => f.endsWith(".js"))) fs.copyFileSync(file, path.join(out, rel(file)));
fs.copyFileSync(path.join(here, "style.css"), path.join(out, "style.css"));
fs.mkdirSync(path.join(out, "training"), { recursive: true });
for (const asset of ["mt.js", "mt.css", "terms.js"])
  fs.copyFileSync(path.join(here, "training", asset), path.join(out, "training", asset));
// The link preview, the same image as the repository's social preview.
fs.copyFileSync(path.join(here, "social.jpg"), path.join(out, "social.jpg"));
// Serve files as they are: no Jekyll processing on GitHub Pages.
fs.writeFileSync(path.join(out, ".nojekyll"), "");
console.log(`built ${specs.length + 1} pages into ${path.relative(process.cwd(), out) || "."}`);
