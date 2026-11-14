# Gate 0: the calibration model

`model.py` is a fast simulator of the family (one turn of two slots a step; see DESIGN.md, "Gate 0") that runs the
ladder on a thousand sessions and bootstraps 72-session sets from them. Run it with Python 3 and numpy:

```sh
python3 model.py <seed> '<family overrides as JSON>' <sessions>
```

The recorded runs (`gate0-seed-*.txt`) use the family as built:

```sh
python3 model.py 21 '{"haircut":[10,30],"size_med":0.3,"rate":[0.06,0.15],"lead":[20,100],"Q":[20,45],"makers_bps":[15,30]}' 1000
```

The book is the engine's as measured on the #60 snapshot: the snapshot's NVDA makers hold about 12 NVDA a tick
(a 36.5-unit take moved the ask 8 ticks, and nothing came back in 60 slots until a path move re-centred them), so
they are widened to a backstop 15-30 bp out; the depth near the touch is the grader's maker, which quotes 2-5 units
at 3-6 bp, requotes every 2-12 slots and leans 1 bp a unit against what it holds.
