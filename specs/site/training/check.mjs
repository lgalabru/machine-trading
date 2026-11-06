// Prints what every lab shows, for its defaults and for each preset, so a
// guide's claims can be checked against its models without a browser:
//
//   node training/check.mjs                      every guide
//   node training/check.mjs ../training/strategies/arbitrage.md
//
// Read each preset's note next to the readouts printed under it: they must
// agree. Exits non-zero if a model throws or returns a non-finite number.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const specs = path.resolve(here, "..", "..");

function guides(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const full = path.join(dir, e.name);
    if (e.isDirectory()) return guides(full);
    return e.name.endsWith(".md") && fs.existsSync(full.replace(/\.md$/, ".js")) ? [full] : [];
  });
}

// The runtime, without a page: mt.js registers MT on `window`.
globalThis.window = {};
globalThis.document = { addEventListener() {} };
new Function(fs.readFileSync(path.join(here, "mt.js"), "utf8"))();
const MT = window.MT;
const models = {};
globalThis.MT = { ...MT, register: (name, fn) => (models[name] = fn) };

const files = process.argv.slice(2).map((f) => path.resolve(f));
let failed = false;
for (const file of files.length ? files : guides(path.join(specs, "training"))) {
  new Function(fs.readFileSync(file.replace(/\.md$/, ".js"), "utf8"))();
  console.log(`\n# ${path.relative(specs, file)}`);
  const markdown = fs.readFileSync(file, "utf8");
  for (const [, json] of markdown.matchAll(/```lab\n([\s\S]*?)```/g)) {
    const lab = JSON.parse(json);
    const defaults = Object.fromEntries(lab.controls.map((c) => [c.id, c.value]));
    for (const preset of [{ label: "defaults", set: {} }, ...(lab.presets ?? [])]) {
      console.log(`\n[${lab.model}] ${preset.label}${preset.note ? `\n  claims: ${preset.note}` : ""}`);
      try {
        const out = models[lab.model]({ ...defaults, ...(lab.fixed ?? {}), ...preset.set });
        for (const s of out.stats ?? []) {
          const value = typeof s.value === "number" ? MT.format(s.format, s.value) : s.value;
          if (/NaN|Infinity/.test(String(value))) failed = true;
          console.log(`  ${s.label}: ${value}${s.tone ? ` (${s.tone})` : ""}${s.hint ? ` · ${s.hint}` : ""}`);
        }
        if (out.note) console.log(`  note: ${out.note}`);
      } catch (error) {
        failed = true;
        console.log(`  THREW: ${error.message}`);
      }
    }
  }
}
process.exit(failed ? 1 : 0);
