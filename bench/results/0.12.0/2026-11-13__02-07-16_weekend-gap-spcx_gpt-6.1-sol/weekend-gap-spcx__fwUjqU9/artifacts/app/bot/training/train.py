"""Train the bundled price forecasters from generated weekend-gap scenarios.

This is an optional development tool, never used by the WASM component.
"""
import os
os.environ.setdefault('OMP_NUM_THREADS', '4')
os.environ.setdefault('OPENBLAS_NUM_THREADS', '1')
import argparse
import glob
import json
from pathlib import Path
import struct
import numpy as np
from sklearn.ensemble import HistGradientBoostingRegressor
from features import scenario_samples


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directories', nargs='+')
    parser.add_argument('--out', type=Path, default=Path('src/models.bin'))
    args = parser.parse_args()
    files = [p for directory in args.directories
             for p in sorted(glob.glob(str(Path(directory) / '*.json'))) ]
    if not files:
        parser.error('No scenario JSON files found')
    rng = np.random.default_rng(18235)
    features, targets, ids = [], [], []
    for k, filename in enumerate(files):
        with open(filename) as f:
            x, y, *_ = scenario_samples(json.load(f), rng)
        features.append(x)
        targets.append(y)
        ids.extend([k] * len(x))
    x = np.concatenate(features)
    y = np.concatenate(targets)
    ids = np.array(ids)
    train = ids % 5 != 0
    models = []
    for i in range(4):
        model = HistGradientBoostingRegressor(
            loss='squared_error', learning_rate=.065, max_iter=380,
            max_leaf_nodes=31, l2_regularization=100,
            min_samples_leaf=180, early_stopping=False, random_state=42)
        model.fit(x[train], y[train, i])
        pred = model.predict(x[~train])
        print(f'horizon {i}: validation RMSE '
              f'{np.mean((pred-y[~train, i])**2)**.5:.6f}', flush=True)
        models.append(model)
    data = bytearray(struct.pack('<I', len(models)))
    for model in models:
        nodes, roots = [], []
        for tree in model._predictors:
            root = len(nodes)
            roots.append(root)
            for n in tree[0].nodes:
                nodes.append((
                    -1 if n['is_leaf'] else int(n['feature_idx']),
                    root + int(n['left']), root + int(n['right']),
                    float(n['num_threshold']), float(n['value'])))
        data += struct.pack('<dII', float(model._baseline_prediction.ravel()[0]),
                            len(roots), len(nodes))
        data += struct.pack('<' + 'I' * len(roots), *roots)
        for node in nodes:
            data += struct.pack('<iIIdd', *node)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_bytes(data)
    print(f'wrote {args.out}: {len(data)} bytes')


if __name__ == '__main__':
    main()
