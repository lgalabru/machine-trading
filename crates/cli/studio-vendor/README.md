# The studio page's editor

`mtx studio`'s page (`../src/studio.html`) edits code with CodeMirror 6,
which ships as many small packages. This directory bundles the ones the
page uses into one module, `../src/studio/codemirror.js`, which `mtx`
embeds and serves at `/studio/codemirror.js`. The bundle is committed, so
building `mtx` needs no Node.

To change what the page imports, or to take newer versions:

```sh
npm install
npm run build
```

`codemirror.mjs` lists the exports.
