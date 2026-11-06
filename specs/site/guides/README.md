# Guides: how to write one

A guide explains one instrument to people: what it is, how it gains
and loses, its parameters, its risks, and when to use it. Guides live in
`specs/guides/instruments/<kind>.md`, next to the spec they explain
(`specs/instruments/<kind>.md`), and are published at
`guides/instruments/<kind>.html`. The specs are normative; a guide teaches
them and never adds a rule.

A guide is two files:

- `<kind>.md`: the text, in Markdown, with callouts and labs.
- `<kind>.js`: the models its labs draw, as pure functions.

`build.mjs` renders the Markdown like any spec, with numbered sections and a
table of contents, and loads `mt.js` and `mt.css` (this folder) and the
guide's own script on the page.

## Kinds of guide

- **Instruments**, in `specs/guides/instruments/<kind>.md`: what a thing
  you can hold is, and how it gains and loses. The outline below.
- **Strategies**, in `specs/guides/strategies/<name>.md`: a repeatable way
  of trading instruments. Same outline, with "What it is" explaining where
  the profit comes from and what it costs to capture, "Parameters" listing
  what a bot has to decide before it trades, and "Under a mandate" saying
  whether the mandate can express it yet. Use the instrument guides' running
  example ($1,000, SPCX at $100) so readers can carry their numbers across.

## The outline

Copy `specs/guides/instruments/perp.md` and keep its sections, in order, so
every instrument reads the same:

1. `:::glance`, then `:::eli5`, before the first section.
2. **What it is**: the mechanics, with a lab for how it gains and loses.
3. One section per mechanism that can hurt: margin, funding, expiry, and so
   on, each with a lab.
4. **Parameters**: a table of who sets each one (the mandate, the venue
   profile, the venue) and what it means.
5. **Risks**: a `:::group` of `::risk` callouts.
6. **When to use it, and when not to**: a `:::group` of `::use` and `::avoid`.
7. **Scenarios**: a lab playing a position through made-up markets, under a
   mandate.
8. **Trading it under a mandate**: how the mandate's limits apply to it.
9. **Check yourself**: `:::check` questions.
10. **Glossary**: a pointer to the [glossary](../../guides/glossary.md), not a table.

Start the title with `Guide: `, and set `Status: guide`.

## Callouts

```
:::kind Optional title
Markdown.
:::
```

| Kind | For |
| --- | --- |
| `glance` | The instrument in a few bullets, laid out as a grid. |
| `eli5` | The instrument explained with no jargon at all. |
| `use`, `avoid` | When to use it, and when not to. |
| `risk` | One risk, named in the title. |
| `note`, `example` | An aside, or a worked example. |
| `history` | Where a name or an instrument comes from, when there is a story worth telling. Don't force one. |
| `check` | A question, in the title, whose answer is hidden until clicked. |

To lay callouts side by side, wrap them in `:::group`, and open and close each
one inside it with `::` instead of `:::`.

## The glossary

`specs/guides/glossary.md` defines every term the guides use, one
`### Term` per entry, grouped under `##` topics. In every guide pages the
first mention of a term in each section is underlined, and shows its
definition on hover or tap (`terms.js`). The definition is the entry's first
paragraph, so make that paragraph stand alone.

Two optional lines under an entry's heading tune the matching, and the page
doesn't show them:

```
### Long

Also: longs, going long, long position
Not: as long as, how long
```

- `Also:` lists the other forms that mean the term: plurals, verbs, phrases.
- `Not:` lists phrases that contain a form but mean something else.

When a guide uses a new term, add it to the glossary rather than defining it
in the guide.

## Labs

A lab is a fenced block of JSON with the language `lab`:

````
```lab
{
  "model": "perp.payoff",
  "title": "What you make or lose when you close",
  "lede": "One line on what to do with it.",
  "controls": [
    { "id": "side", "label": "Side", "type": "choice", "options": [["long", "Long"], ["short", "Short"]], "value": "long" },
    { "id": "leverage", "label": "Leverage", "min": 1, "max": 25, "step": 0.5, "value": 5, "format": "x", "hint": "Optional help under the slider." }
  ],
  "presets": [
    { "label": "Aggressive long", "set": { "leverage": 20 }, "note": "Shown when the preset is picked." }
  ],
  "caption": "Markdown under the figure: what the reader should take away."
}
```
````

- **Controls** are sliders (`min`, `max`, `step`, `value`, `format`) or, with
  `"type": "choice"`, segmented buttons (`options` as `[value, label]` pairs).
- **Presets** are named cases: picking one resets every control to its
  default and then applies `set`. Write their `note` as what happens, so a
  reader learns from clicking through them.
- The build checks every lab: valid JSON, a model the guide's script
  registers, values inside their ranges, presets that set real controls. A
  broken lab fails the build, not the page.

## Models

A model turns the controls' values into what to draw. It is a pure function
and knows nothing about the page:

```js
MT.register("perp.payoff", (p) => ({
  stats: [{ label: "Margin posted", value: 1000, format: "usd", tone: "good", hint: "notional ÷ 5×" }],
  note: "Plain text under the readouts, such as what just happened.",
  panels: [
    {
      title: "Optional, for labs with several panels",
      height: 200,
      x: { label: "Price when you close", format: "price" },
      y: { label: "PnL", format: "+usd", zero: true },
      series: [{ name: "PnL", points: [[90, -500], [110, 500]], fill: "sign" }],
      rules: [{ axis: "x", at: 100, label: "Entry" }],
      bands: [{ axis: "x", from: -Infinity, to: 82, tone: "danger", label: "Liquidated" }],
      dots: [{ x: 82, y: -1000, label: "Liquidated", tone: "danger" }],
    },
  ],
}));
```

- **Stats** are the readout tiles. `tone` is `good`, `bad` or `warn`, and
  always comes with an icon, so it never relies on colour alone. A `value`
  that is a number is formatted with `format`; a string is shown as it is.
- **Panels** are charts. Several panels share their x axis and crosshair, so
  hovering one reads them all at the same x. Use several panels rather than
  two y axes on one chart.
- **Series** are lines, with points sorted by x. `fill: "sign"` shades the
  area above zero as profit and below as loss. `color` is `s1`, `s2`, `s3`
  (the default order) or `muted`; `dash: true` for a reference line.
- **Rules** are labelled reference lines, **bands** shaded ranges (`danger`
  is also hatched), and **dots** labelled events.
- **Formats** are `num`, `usd`, `usd0`, `+usd`, `price`, `pct`, `pct0`,
  `+pct`, `bp`, `x`, `h` and `d`. Percentages are fractions: `0.05` is 5%.
  `MT.format(name, value)` formats in a model, and `MT.range(from, to, n)`
  gives evenly spaced x values.

Every lab also gets a crosshair with a tooltip, keyboard reading with the
arrow keys, and a data table under the chart, without the model doing
anything.

The numbers in a model are a teaching model, and the guide says so. Use the
spec's terms and limits, and leave a venue's exact margin and funding rules
to its venue profile. Before publishing, run `npm run check`, which prints
every lab's readouts under each preset's claim, and make the two agree.

The approach behind the guides, and how to check and publish one, is in the
`writing-guides` skill (`.claude/skills/writing-guides/SKILL.md`).
