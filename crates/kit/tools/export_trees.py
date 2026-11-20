#!/usr/bin/env python3
"""Write a fitted tree ensemble in the format `mtx_kit::trees::Trees` loads.

From Python:

    from export_trees import export, check
    data = export(model)                 # bytes; raises Unsupported
    check(model, data, X)                # the bytes predict as the model does
    open("model.bin", "wb").write(data)

From a shell, for a pickled scikit-learn model or a LightGBM model file:

    python export_trees.py model.pkl model.bin [--check features.npy]
    python export_trees.py model.txt model.bin [--check features.npy]

Then, in the strategy: `Trees::from_bytes(include_bytes!("../model.bin"))`.

Supported, both for regression with a single output:

- scikit-learn `HistGradientBoostingRegressor` with an identity link
  (`loss` of `squared_error`, `absolute_error` or `quantile`).
- LightGBM `LGBMRegressor` or `Booster` with an objective whose raw score is
  the prediction (`regression`, `regression_l1`, `huber`, `fair`,
  `quantile`, `mape`).

Refused, with an error that says which, since the format cannot hold them:

- Categorical splits.
- Missing-value routing. The format sends a feature left when it is at most
  the threshold and has no rule for NaN, so `Trees::predict` refuses NaN.
  A scikit-learn split that only separates missing values, and LightGBM's
  `zero_as_missing`, are refused here. Train on data without NaN.
- Losses and objectives with a link or transform (`poisson`, `gamma`,
  `tweedie`, `binary`, `reg_sqrt`), classifiers, multi-output models,
  LightGBM's linear trees and random-forest averaging.

**Train on float32 features.** `Trees::predict` rounds each feature to
float32 before comparing it to a threshold. So its prediction for `x` is the
Python model's for `x.astype(np.float32)`, bit for bit, and the model's for
`x` itself when it was fitted on float32 features (`X.astype(np.float32)`).
`check` compares against the float32 prediction for that reason.

The format, little-endian (see the module docs of `mtx_kit::trees`):

    u32 tree_count, f64 base, then for each tree:
      u32 node_count, then for each node, 32 bytes:
        u32 feature, u32 is_leaf, f64 threshold, f64 value, u32 left, u32 right
"""

import argparse
import math
import pickle
import struct
import sys

# The limits `mtx_kit::trees` enforces: an export past them would not load.
TREE_COUNT_MAX = 16_384
TREE_NODE_COUNT_MAX = 65_535
NODE_COUNT_MAX = 1_048_576
FEATURE_COUNT_MAX = 4_096

HEADER = struct.Struct("<Id")
TREE_HEADER = struct.Struct("<I")
NODE = struct.Struct("<IIddII")

# LightGBM objectives whose raw score is the prediction.
LIGHTGBM_OBJECTIVES = {
    "regression",
    "regression_l1",
    "huber",
    "fair",
    "quantile",
    "mape",
}
# LightGBM's `missing_type` values that leave a finite feature to the plain
# comparison: `None` has no routing, and `NaN` routes NaN alone.
LIGHTGBM_MISSING_TYPES = {"None", "NaN"}


class Unsupported(Exception):
    """The model uses something the format cannot represent."""


def export(model):
    """The model as bytes, for a supported scikit-learn or LightGBM model."""
    name = type(model).__name__
    if name == "HistGradientBoostingRegressor":
        base, trees = _sklearn_trees(model)
    elif name in ("LGBMRegressor", "Booster"):
        base, trees = _lightgbm_trees(model)
    else:
        raise Unsupported(
            f"{name} is not supported: export a scikit-learn "
            "HistGradientBoostingRegressor or a LightGBM regression model"
        )
    return encode(base, trees)


def encode(base, trees):
    """The format's bytes. A tree is a list of nodes in any order with node 0
    the root; a node is `("leaf", value)` or
    `("split", feature, threshold, left, right)`."""
    if not math.isfinite(base):
        raise Unsupported("the base prediction is not finite")
    if len(trees) > TREE_COUNT_MAX:
        raise Unsupported(f"{len(trees)} trees is over the maximum, {TREE_COUNT_MAX}")
    node_count = sum(len(tree) for tree in trees)
    if node_count > NODE_COUNT_MAX:
        raise Unsupported(f"{node_count} nodes is over the maximum, {NODE_COUNT_MAX}")
    parts = [HEADER.pack(len(trees), base)]
    for index, tree in enumerate(trees):
        if not 1 <= len(tree) <= TREE_NODE_COUNT_MAX:
            raise Unsupported(f"tree {index} holds {len(tree)} nodes: over the limit, or none")
        parts.append(TREE_HEADER.pack(len(tree)))
        for node in tree:
            parts.append(_encode_node(node, index, len(tree)))
    data = b"".join(parts)
    assert len(data) == HEADER.size + TREE_HEADER.size * len(trees) + NODE.size * node_count
    return data


def _encode_node(node, tree_index, tree_len):
    if node[0] == "leaf":
        (_, value) = node
        if not math.isfinite(value):
            raise Unsupported(f"tree {tree_index} has a leaf value that is not finite")
        return NODE.pack(0, 1, 0.0, value, 0, 0)
    assert node[0] == "split", node
    (_, feature, threshold, left, right) = node
    if not 0 <= feature < FEATURE_COUNT_MAX:
        raise Unsupported(f"feature {feature} is over the maximum, {FEATURE_COUNT_MAX}")
    if not math.isfinite(threshold):
        raise Unsupported(
            f"tree {tree_index} has a split whose threshold is not finite: it "
            "separates missing values, which the format cannot route"
        )
    assert 0 <= left < tree_len and 0 <= right < tree_len and left != right
    return NODE.pack(feature, 0, threshold, 0.0, left, right)


def _sklearn_trees(model):
    link = type(model._loss.link).__name__
    if link != "IdentityLink":
        raise Unsupported(
            f"loss {model.loss!r} predicts through a {link}: only losses with "
            "an identity link export (squared_error, absolute_error, quantile)"
        )
    if model.n_trees_per_iteration_ != 1:
        raise Unsupported("the model has more than one output")
    base = float(model._baseline_prediction.ravel()[0])
    trees = []
    for (predictor,) in model._predictors:
        nodes = predictor.nodes
        if nodes["is_categorical"].any():
            raise Unsupported("the model has categorical splits")
        tree = []
        for node in nodes:
            if node["is_leaf"]:
                tree.append(("leaf", float(node["value"])))
            else:
                tree.append(
                    (
                        "split",
                        int(node["feature_idx"]),
                        float(node["num_threshold"]),
                        int(node["left"]),
                        int(node["right"]),
                    )
                )
        trees.append(tree)
    return base, trees


def _lightgbm_trees(model):
    booster = getattr(model, "booster_", model)
    dump = booster.dump_model()
    objective = dump.get("objective", "").split()
    if not objective or objective[0] not in LIGHTGBM_OBJECTIVES or "sqrt" in objective:
        raise Unsupported(
            f"objective {' '.join(objective)!r} is not supported: only "
            f"{sorted(LIGHTGBM_OBJECTIVES)}, without reg_sqrt"
        )
    if dump["num_class"] != 1 or dump["num_tree_per_iteration"] != 1:
        raise Unsupported("the model has more than one output")
    if dump.get("average_output", False):
        raise Unsupported("random-forest averaging is not supported")
    trees = [_lightgbm_tree(info["tree_structure"]) for info in dump["tree_info"]]
    # LightGBM folds the starting score into the first tree's leaves.
    return 0.0, trees


def _lightgbm_tree(root):
    """One tree in preorder, from LightGBM's nested dump. An explicit stack,
    bounded by the node limit: a dump is as deep as the tree is."""
    tree = []
    # (the dumped node, the index of the parent split, which child it is)
    pending = [(root, None, None)]
    while pending:
        if len(tree) >= TREE_NODE_COUNT_MAX:
            raise Unsupported(f"a tree holds over {TREE_NODE_COUNT_MAX} nodes")
        dumped, parent, side = pending.pop()
        index = len(tree)
        if parent is not None:
            tree[parent][side] = index
        if "leaf_value" in dumped:
            if "leaf_coeff" in dumped or "leaf_features" in dumped:
                raise Unsupported("linear trees are not supported")
            tree.append(["leaf", float(dumped["leaf_value"])])
            continue
        _check_lightgbm_split(dumped)
        tree.append(["split", int(dumped["split_feature"]), float(dumped["threshold"]), None, None])
        # The left child is popped first, so it follows its parent.
        pending.append((dumped["right_child"], index, 4))
        pending.append((dumped["left_child"], index, 3))
    return [tuple(node) for node in tree]


def _check_lightgbm_split(dumped):
    if dumped["decision_type"] != "<=":
        raise Unsupported(
            f"a split decides by {dumped['decision_type']!r}: categorical "
            "splits are not supported"
        )
    if dumped["missing_type"] not in LIGHTGBM_MISSING_TYPES:
        raise Unsupported(
            f"a split routes missing values as {dumped['missing_type']!r} "
            "(zero_as_missing), which the format cannot represent"
        )


def predict(data, features):
    """What `Trees::predict` returns for one row of features: this file's
    reference reading of the format, in the same order of additions."""
    count, prediction = HEADER.unpack_from(data, 0)
    at = HEADER.size
    for _ in range(count):
        (node_count,) = TREE_HEADER.unpack_from(data, at)
        at += TREE_HEADER.size
        index = 0
        for _ in range(node_count):
            feature, is_leaf, threshold, value, left, right = NODE.unpack_from(
                data, at + NODE.size * index
            )
            if is_leaf:
                prediction += value
                break
            index = left if _as_float32(features[feature]) <= threshold else right
        else:
            raise ValueError("a tree does not reach a leaf")
        at += NODE.size * node_count
    if at != len(data):
        raise ValueError("bytes follow the last tree")
    return prediction


def _as_float32(value):
    """The value rounded to float32, as a Python float. Past float32's range
    it rounds to infinity, as Rust's `as f32` does."""
    try:
        return struct.unpack("<f", struct.pack("<f", value))[0]
    except OverflowError:
        return math.copysign(math.inf, value)


def check(model, data, rows, tolerance=1e-9):
    """Raises unless the exported bytes predict every row as the model
    predicts it rounded to float32. `rows` is a 2-D array without NaN."""
    import numpy as np

    rows = np.asarray(rows, dtype=np.float64)
    if np.isnan(rows).any():
        raise ValueError("the rows hold NaN, which Trees::predict refuses")
    # A value past float32's range becomes infinity: meant, not an accident.
    with np.errstate(over="ignore"):
        expected = model.predict(rows.astype(np.float32))
    worst = 0.0
    for row, want in zip(rows, expected):
        worst = max(worst, abs(predict(data, row) - float(want)))
    if worst > tolerance:
        raise AssertionError(
            f"the export differs from the model by up to {worst}: do not ship it"
        )
    return worst


def _load(path):
    """A LightGBM text model, or a pickled model of either library."""
    with open(path, "rb") as file:
        head = file.read(4)
    if head == b"tree":
        import lightgbm

        return lightgbm.Booster(model_file=path)
    with open(path, "rb") as file:
        # A pickle runs code when loaded: only load a model you trained.
        return pickle.load(file)


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("model", help="a pickled model, or a LightGBM model file")
    parser.add_argument("out", help="where to write the bytes, e.g. model.bin")
    parser.add_argument("--check", metavar="NPY", help="rows to compare predictions on")
    args = parser.parse_args(argv)
    model = _load(args.model)
    try:
        data = export(model)
    except Unsupported as error:
        print(f"not exported: {error}", file=sys.stderr)
        return 1
    if args.check:
        import numpy as np

        worst = check(model, data, np.load(args.check))
        print(f"checked: the export and the model agree within {worst:.3g}")
    with open(args.out, "wb") as file:
        file.write(data)
    print(f"wrote {args.out}: {len(data)} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
