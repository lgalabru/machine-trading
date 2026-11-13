# Gate 0: the calibration model

`model.py` is a fast simulator of the family (one-second steps; see DESIGN.md, "Gate 0") that runs the ladder on
hundreds of sessions. Run it with Python 3 and numpy:

```sh
python3 model.py <seed> '<family overrides as JSON>' <training sessions>
```

The recorded runs (`gate0-seed-*.txt`) use the family as built (switches at the model's one-minute blocks):

```sh
python3 model.py 31 '{"period":[150,600],"lam":[0.0001,0.012],"mult":[0.85,1.7],"wsd":0.4,"pw":1.5,"pn":[0.02,0.05],"block_switch":1,"q_cs":0.0014167,"q_sc":0.0035417}' 300
```

`gate0-perslot-switch-seed-*.txt` are the same family with switches allowed every second (the design before the
infra settled on block-end switching). Each run fits the tuned rungs on 300 sessions and scores them on 600 others.
