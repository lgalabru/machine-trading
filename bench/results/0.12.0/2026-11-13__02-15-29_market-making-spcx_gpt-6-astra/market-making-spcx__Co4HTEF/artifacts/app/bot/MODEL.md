# Flow model

The model was fitted on generated scenarios from the public `market-making` family, not on hidden grading scenarios. Scenario identifiers and seeds are never strategy inputs.

The final model used 5,000 generated scenarios (generation seed 29148002), split into 4,000 fitting sessions and 1,000 held-out sessions with split seed 471092. It uses 200 gradient-boosted trees, at most 16 leaves and depth 5, with learning rate 0.06 and a minimum leaf population of 1,500 samples. The fitted constants are embedded in `src/model.rs`; no Python packages are needed at runtime.

The label is an upward jump during the next 40 slots. Each fitting session is also reflected: buys and sells exchange roles, price drift changes sign, and the label becomes a downward jump. At runtime, the same model is evaluated in both orientations and the probability difference supplies the directional signal.

The 20 features, in source order, are:

1. Buy and sell order counts in each of the last 4, 6, 10, 16, 24, and 32 slots (12 features).
2. Smoothed cumulative buy and sell rates (2 features).
3. Mean and standard deviation of observed non-jump price changes (2 features).
4. Elapsed slots, remaining slots, and slots since the last observed jump, each capped at 200 (3 features).
5. Number of jumps already observed (1 feature).

Only completed blocks contribute order counts. The current oracle supplies price observations. The entry threshold also accounts for the initial visible spread. Time gates use the task's stated minimum distance between jumps and the session endpoints.
