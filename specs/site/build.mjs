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
const SOURCE = "https://github.com/lgalabru/machine-trading/tree/main/specs";
// Folders of the index after the core spec, in order.
const FOLDERS = [
  { dir: "instruments", title: "Instruments" },
  { dir: "venues", title: "Venues" },
  { dir: "examples", title: "Examples" },
];

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

/** A spec's body as HTML, with its sections numbered, and its table of contents. */
function render(markdown, page) {
  const toc = [];
  let section = 0;
  let subsection = 0;
  const marked = new Marked({
    gfm: true,
    renderer: {
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

function layout({ page, title, head, body }) {
  const index = page === "index.html";
  const trail = [`<a href="${link(page, "index.html")}">${NAME}</a>`]
    .concat(
      page
        .split("/")
        .slice(0, -1)
        .map((dir) => `<a href="${link(page, "index.html")}#${dir}">${dir}</a>`),
    )
    .join('<span class="sep">/</span>');
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escape(index ? `${NAME} Specifications` : `${title} · ${NAME}`)}</title>
<meta name="description" content="${NAME}: what an agent may trade on someone else's behalf, and the limits it must stay within.">
<link rel="stylesheet" href="${link(page, "style.css")}">
</head>
<body class="${index ? "index" : "spec"}">
${index ? "" : `<p class="trail">${trail}</p>`}
${head}
${body}
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

for (const spec of specs) {
  const { body, toc } = render(spec.markdown, spec.page);
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
    .filter((f) => path.posix.dirname(rel(f)) === dir)
    .sort()
    .map((f) => {
      const spec = specs.find((s) => s.file === f);
      if (!spec) return { href: rel(f), title: path.basename(f), note: "JSON" };
      const short = spec.title.replace(/^.*?:\s*/, "").replace(/,\s*v\d+$/, "");
      const name = short.charAt(0).toUpperCase() + short.slice(1);
      return { href: spec.page, title: name, note: escape(spec.status ?? "") };
    });
  tree += `<div class="folder" id="${dir}"><span class="folder-icon">~/</span>${title}</div>`;
  tree += children.map((c, i) => entry(c, i === children.length - 1)).join("");
}
write(
  "index.html",
  layout({
    page: "index.html",
    title: NAME,
    head: `<h1>${NAME} Specifications</h1><p class="header-links"><a href="${SOURCE}">Source</a><span class="sep">·</span><a href="mandate.html">Mandate v0</a></p>`,
    body: `<div class="intro">${intro}</div><div class="tree">${tree}</div>`,
  }),
);

for (const file of files.filter((f) => f.endsWith(".json"))) {
  fs.mkdirSync(path.dirname(path.join(out, rel(file))), { recursive: true });
  fs.copyFileSync(file, path.join(out, rel(file)));
}
fs.copyFileSync(path.join(here, "style.css"), path.join(out, "style.css"));
// Serve files as they are: no Jekyll processing on GitHub Pages.
fs.writeFileSync(path.join(out, ".nojekyll"), "");
console.log(`built ${specs.length + 1} pages into ${path.relative(process.cwd(), out) || "."}`);
