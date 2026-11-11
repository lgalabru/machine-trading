The embedded model is a 400-tree gradient-boosted regression ensemble, maximum depth 7. It predicts a discounted sum of future oracle-price changes in percentage points, with a discount factor of 0.85 per half-second step. All 77 input features use only prices already observed and the session clock supplied by the mandate.

Training used 28,000 scenarios generated with the task's public `mtx scenario generate` command: 8,000 from seed 761353 and 20,000 from seed 620915. Training retained alternate time steps to reduce redundant observations. Another 2,000 scenarios were held out from fitting. The fresh sandbox check used 96 scenarios from seed 713159; the 72 stress cases came from the held-out set. No hidden scenarios or future session data are accessed by the executable.

The trading threshold is 0.1125 percentage points in either direction, with a symmetric holding band to avoid repeatedly paying the spread. The risk layer can shrink or override every model position.

`model.bin` contains a little-endian tree count, base score, node count, tree-root indices, and fixed-size nodes (left child, right child, feature index, threshold or leaf value). The file is included by Rust at compile time. The parity fixture checks the Rust features and evaluator against predictions from the training implementation.
