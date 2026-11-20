//! A tree ensemble a strategy embeds: gradient-boosted regression trees
//! trained offline, loaded from bytes, evaluated in a step.
//!
//! ```
//! use mtx_kit::trees::{Trees, TreesError};
//!
//! /// `model` is the embedded file: `include_bytes!("../model.bin")`.
//! fn predict(model: &[u8], features: &[f64]) -> Result<f64, TreesError> {
//!     let trees = Trees::from_bytes(model)?; // once, in `start`
//!     trees.predict(features) // every step
//! }
//! # // The smallest model: no tree, a base of 1.5.
//! # let mut model = 0u32.to_le_bytes().to_vec();
//! # model.extend(1.5f64.to_le_bytes());
//! # assert_eq!(predict(&model, &[]), Ok(1.5));
//! ```
//!
//! `tools/export_trees.py`, next to this crate's `src`, writes the format
//! from a fitted scikit-learn `HistGradientBoostingRegressor` or a LightGBM
//! regression model.
//!
//! # The prediction
//!
//! The prediction is the base plus one leaf value a tree. A tree is walked
//! from its root, node 0: at a split, the walk goes **left when
//! `features[feature] as f32 as f64 <= threshold`**, and right otherwise.
//!
//! The feature is rounded to `f32` before it is compared, as scikit-learn's
//! classic trees do. So `predict(x)` equals the Python model's prediction on
//! `x.astype(np.float32)`, exactly. On `x` itself the two can differ, even
//! for a model trained on `float32` features: scikit-learn's histogram
//! gradient boosting and LightGBM compare the unrounded `f64`, so a value
//! within an `f32` rounding of a threshold can go the other way. Round the
//! features to `f32` in the training pipeline too (the exporter asks for
//! `float32` training data), and the two agree. Seven significant digits
//! are plenty for a feature;
//! they are not for a raw price next to a threshold, so feed returns and
//! ratios, not prices.
//!
//! A feature that is NaN is refused: the format has no missing-value
//! routing. An infinite feature goes the way the comparison says.
//!
//! # The binary format
//!
//! Little-endian, no padding, no header beyond the two fields below:
//!
//! ```text
//! u32  tree_count
//! f64  base                  added to every prediction
//! tree_count times:
//!   u32  node_count          at least 1; node 0 is the root
//!   node_count times, 32 bytes each:
//!     u32  feature           a split's index into the features; 0 in a leaf
//!     u32  is_leaf           1 for a leaf, 0 for a split
//!     f64  threshold         a split's threshold; unused in a leaf
//!     f64  value             a leaf's value; unused in a split
//!     u32  left              a split's child when feature <= threshold
//!     u32  right             a split's child otherwise
//! ```
//!
//! Child indexes count from the tree's own node 0. The format doesn't say
//! how many features the model takes: [`Trees::feature_count`] is the
//! highest feature a split reads, plus one, and [`Trees::predict`] refuses
//! a shorter slice.
//!
//! [`Trees::from_bytes`] checks everything [`Trees::predict`] relies on, so
//! bytes from anywhere are safe to load: it returns an error, and never
//! panics, on input that is truncated, too long, over the limits below, or
//! not a set of trees (a child out of range, a node reached twice or never,
//! a threshold or leaf value that is not finite).

/// The most trees a model may hold. The largest model a reference has
/// shipped holds 1,900; this leaves room for eight times that.
pub const TREE_COUNT_MAX: usize = 16_384;

/// The most nodes one tree may hold: a full binary tree of depth 15. The
/// libraries' defaults grow 31 leaves a tree, 61 nodes.
pub const TREE_NODE_COUNT_MAX: usize = 65_535;

/// The most nodes a model may hold in all, 32 MiB of them. The largest
/// model a reference has shipped holds 188,082 nodes, 6 MB.
pub const NODE_COUNT_MAX: usize = 1_048_576;

/// The most features a model may read: a split's feature index is under
/// this. Models here read tens of features; an index far above is a
/// corrupted file, and would make `predict` demand a slice as long.
pub const FEATURE_COUNT_MAX: usize = 4_096;

/// The longest input [`Trees::from_bytes`] reads: a model at every limit.
pub const BYTES_MAX: usize =
    HEADER_BYTES + TREE_COUNT_MAX * TREE_HEADER_BYTES + NODE_COUNT_MAX * NODE_BYTES;

/// `tree_count` and `base`.
const HEADER_BYTES: usize = 4 + 8;
/// `node_count`.
const TREE_HEADER_BYTES: usize = 4;
/// One node: two `u32`, two `f64`, two `u32`.
const NODE_BYTES: usize = 32;

/// Why bytes are not a model, or features cannot be predicted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TreesError {
    /// The input is longer than [`BYTES_MAX`].
    #[error("{bytes} bytes is over the maximum, {BYTES_MAX}")]
    TooLong {
        /// The input's length.
        bytes: usize,
    },
    /// The input ends before the model does.
    #[error("the input ends at byte {bytes}, inside the model")]
    Truncated {
        /// The input's length.
        bytes: usize,
    },
    /// Bytes are left after the last tree.
    #[error("{extra} bytes follow the last tree")]
    TrailingBytes {
        /// How many.
        extra: usize,
    },
    /// More trees than [`TREE_COUNT_MAX`].
    #[error("{trees} trees is over the maximum, {TREE_COUNT_MAX}")]
    TooManyTrees {
        /// The count the input declares.
        trees: usize,
    },
    /// A tree with no node, or more than [`TREE_NODE_COUNT_MAX`], or the
    /// model past [`NODE_COUNT_MAX`] in all.
    #[error("tree {tree} declares {nodes} nodes: over a limit, or none")]
    NodeCountOutOfRange {
        /// The tree, from 0.
        tree: usize,
        /// The count it declares.
        nodes: usize,
    },
    /// The base is NaN or infinite.
    #[error("the base is not finite")]
    BaseNotFinite,
    /// A node's `is_leaf` is neither 0 nor 1.
    #[error("tree {tree}, node {node}: is_leaf is neither 0 nor 1")]
    LeafFlagInvalid {
        /// The tree, from 0.
        tree: usize,
        /// The node, from the tree's 0.
        node: usize,
    },
    /// A split's child index is not a node of its tree, or both children
    /// are the same node.
    #[error("tree {tree}, node {node}: a child is out of range, or both are the same")]
    ChildInvalid {
        /// The tree, from 0.
        tree: usize,
        /// The node, from the tree's 0.
        node: usize,
    },
    /// A node has two parents, or is its own ancestor: not a tree.
    #[error("tree {tree}, node {node}: reached twice, so this is not a tree")]
    NodeReachedTwice {
        /// The tree, from 0.
        tree: usize,
        /// The node, from the tree's 0.
        node: usize,
    },
    /// A node no walk from the root reaches.
    #[error("tree {tree}, node {node}: never reached from the root")]
    NodeUnreachable {
        /// The tree, from 0.
        tree: usize,
        /// The node, from the tree's 0.
        node: usize,
    },
    /// A split reads a feature at or past [`FEATURE_COUNT_MAX`].
    #[error("tree {tree}, node {node}: the feature index is over the maximum, {FEATURE_COUNT_MAX}")]
    FeatureOutOfRange {
        /// The tree, from 0.
        tree: usize,
        /// The node, from the tree's 0.
        node: usize,
    },
    /// A split's threshold or a leaf's value is NaN or infinite.
    #[error("tree {tree}, node {node}: a threshold or leaf value is not finite")]
    NumberNotFinite {
        /// The tree, from 0.
        tree: usize,
        /// The node, from the tree's 0.
        node: usize,
    },
    /// `predict` was given fewer features than the model reads.
    #[error("{given} features given, and the model reads {needed}")]
    FeaturesTooShort {
        /// The slice's length.
        given: usize,
        /// [`Trees::feature_count`].
        needed: usize,
    },
    /// `predict` was given a NaN in a feature the model reads.
    #[error("feature {feature} is NaN")]
    FeatureNotANumber {
        /// The feature's index.
        feature: usize,
    },
}

/// A tree ensemble, checked when loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct Trees {
    base: f64,
    /// Every tree's nodes, one tree after another.
    nodes: Vec<Node>,
    /// Where each tree's nodes are in `nodes`.
    spans: Vec<Span>,
    /// The highest feature a split reads, plus one.
    feature_count: usize,
}

/// One node, as the format stores it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Node {
    feature: u32,
    is_leaf: bool,
    threshold: f64,
    value: f64,
    left: u32,
    right: u32,
}

/// One tree's nodes in [`Trees::nodes`].
#[derive(Debug, Clone, Copy, PartialEq)]
struct Span {
    start: usize,
    len: usize,
}

impl Trees {
    /// Loads a model from the format in the module's documentation,
    /// checking all of it. Bad bytes are an error, never a panic.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TreesError> {
        if bytes.len() > BYTES_MAX {
            return Err(TreesError::TooLong { bytes: bytes.len() });
        }
        let mut reader = Reader { bytes, at: 0 };
        let tree_count = reader.u32()? as usize;
        if tree_count > TREE_COUNT_MAX {
            return Err(TreesError::TooManyTrees { trees: tree_count });
        }
        let base = reader.f64()?;
        if !base.is_finite() {
            return Err(TreesError::BaseNotFinite);
        }
        let mut trees = Self {
            base,
            nodes: Vec::new(),
            spans: Vec::with_capacity(tree_count),
            feature_count: 0,
        };
        for tree in 0..tree_count {
            trees.read_tree(&mut reader, tree)?;
        }
        let extra = bytes.len() - reader.at;
        if extra != 0 {
            return Err(TreesError::TrailingBytes { extra });
        }
        assert_eq!(trees.spans.len(), tree_count);
        assert!(trees.nodes.len() <= NODE_COUNT_MAX);
        assert!(trees.feature_count <= FEATURE_COUNT_MAX);
        Ok(trees)
    }

    /// The model in the same format: `from_bytes` of it is this model, and
    /// a model loaded from bytes writes those bytes back.
    pub fn to_bytes(&self) -> Vec<u8> {
        let bytes_len = self.bytes_len();
        let mut bytes = Vec::with_capacity(bytes_len);
        // Both counts are under limits that fit a u32, checked at load.
        bytes.extend_from_slice(&(self.spans.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&self.base.to_le_bytes());
        for span in &self.spans {
            bytes.extend_from_slice(&(span.len as u32).to_le_bytes());
            for node in &self.nodes[span.start..span.start + span.len] {
                bytes.extend_from_slice(&node.feature.to_le_bytes());
                bytes.extend_from_slice(&u32::from(node.is_leaf).to_le_bytes());
                bytes.extend_from_slice(&node.threshold.to_le_bytes());
                bytes.extend_from_slice(&node.value.to_le_bytes());
                bytes.extend_from_slice(&node.left.to_le_bytes());
                bytes.extend_from_slice(&node.right.to_le_bytes());
            }
        }
        assert_eq!(bytes.len(), bytes_len);
        bytes
    }

    /// The model's prediction: the base plus each tree's leaf. `features`
    /// holds at least [`feature_count`](Self::feature_count) values, none of
    /// those NaN; more are ignored.
    pub fn predict(&self, features: &[f64]) -> Result<f64, TreesError> {
        if features.len() < self.feature_count {
            return Err(TreesError::FeaturesTooShort {
                given: features.len(),
                needed: self.feature_count,
            });
        }
        let read = &features[..self.feature_count];
        if let Some(feature) = read.iter().position(|value| value.is_nan()) {
            return Err(TreesError::FeatureNotANumber { feature });
        }
        let mut prediction = self.base;
        for span in &self.spans {
            prediction += leaf_value(&self.nodes[span.start..span.start + span.len], read);
        }
        Ok(prediction)
    }

    /// How many features the model reads: the highest feature index of a
    /// split, plus one. Zero for a model of leaves alone. Assert in a test
    /// that it equals the number of features your strategy builds.
    pub fn feature_count(&self) -> usize {
        self.feature_count
    }

    /// How many trees the model holds.
    pub fn tree_count(&self) -> usize {
        self.spans.len()
    }

    /// How many nodes the model holds, over all trees.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// What every prediction starts from.
    pub fn base(&self) -> f64 {
        self.base
    }

    /// The length of [`to_bytes`](Self::to_bytes).
    fn bytes_len(&self) -> usize {
        HEADER_BYTES + self.spans.len() * TREE_HEADER_BYTES + self.nodes.len() * NODE_BYTES
    }

    /// Reads tree number `tree`, checks it, and appends it.
    fn read_tree(&mut self, reader: &mut Reader<'_>, tree: usize) -> Result<(), TreesError> {
        let node_count = reader.u32()? as usize;
        let is_in_range = (1..=TREE_NODE_COUNT_MAX).contains(&node_count)
            && self.nodes.len() + node_count <= NODE_COUNT_MAX;
        if !is_in_range {
            return Err(TreesError::NodeCountOutOfRange {
                tree,
                nodes: node_count,
            });
        }
        // Before reserving: a count the input cannot back is a lie.
        reader.require(node_count * NODE_BYTES)?;
        let start = self.nodes.len();
        self.nodes.reserve(node_count);
        for node in 0..node_count {
            let read = reader.node(tree, node)?;
            self.nodes.push(read);
        }
        let feature_count = check_tree(&self.nodes[start..], tree)?;
        self.feature_count = self.feature_count.max(feature_count);
        self.spans.push(Span {
            start,
            len: node_count,
        });
        Ok(())
    }
}

/// The value of the leaf `features` lead to. `nodes` passed [`check_tree`],
/// so every child is in range and a walk ends at a leaf within `nodes.len()`
/// steps; `features` is as long as the model reads.
fn leaf_value(nodes: &[Node], features: &[f64]) -> f64 {
    let mut index = 0;
    for _ in 0..nodes.len() {
        let node = &nodes[index];
        if node.is_leaf {
            return node.value;
        }
        // Rounded to f32 first: the comparison the format is defined by.
        let feature = features[node.feature as usize] as f32 as f64;
        index = if feature <= node.threshold {
            node.left as usize
        } else {
            node.right as usize
        };
    }
    unreachable!("a checked tree reaches a leaf within its node count")
}

/// Checks that `nodes` is one tree rooted at node 0: every node reached
/// exactly once, every child in range, every number used finite. Returns
/// the highest feature a split reads, plus one.
fn check_tree(nodes: &[Node], tree: usize) -> Result<usize, TreesError> {
    assert!(!nodes.is_empty() && nodes.len() <= TREE_NODE_COUNT_MAX);
    let mut is_reached = vec![false; nodes.len()];
    is_reached[0] = true;
    // Each node is pushed once, when first reached, so the stack and the
    // loop are both bounded by the node count.
    let mut pending = Vec::with_capacity(nodes.len());
    pending.push(0usize);
    let mut feature_count = 0;
    let mut visited_count = 0;
    while let Some(node) = pending.pop() {
        visited_count += 1;
        assert!(visited_count <= nodes.len(), "each node is visited once");
        let read = &nodes[node];
        if read.is_leaf {
            if !read.value.is_finite() {
                return Err(TreesError::NumberNotFinite { tree, node });
            }
            continue;
        }
        feature_count = feature_count.max(check_split(read, nodes.len(), tree, node)?);
        for child in [read.right as usize, read.left as usize] {
            if is_reached[child] {
                return Err(TreesError::NodeReachedTwice { tree, node: child });
            }
            is_reached[child] = true;
            pending.push(child);
        }
    }
    if let Some(node) = is_reached.iter().position(|is_reached| !is_reached) {
        return Err(TreesError::NodeUnreachable { tree, node });
    }
    assert_eq!(visited_count, nodes.len());
    Ok(feature_count)
}

/// Checks one split's own fields. Returns its feature index plus one.
fn check_split(
    split: &Node,
    node_count: usize,
    tree: usize,
    node: usize,
) -> Result<usize, TreesError> {
    assert!(!split.is_leaf);
    let (left, right) = (split.left as usize, split.right as usize);
    if left >= node_count || right >= node_count || left == right {
        return Err(TreesError::ChildInvalid { tree, node });
    }
    if split.feature as usize >= FEATURE_COUNT_MAX {
        return Err(TreesError::FeatureOutOfRange { tree, node });
    }
    if !split.threshold.is_finite() {
        return Err(TreesError::NumberNotFinite { tree, node });
    }
    Ok(split.feature as usize + 1)
}

/// A cursor over the input that fails, instead of panicking, at its end.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    /// Fails unless `count` more bytes are left.
    fn require(&self, count: usize) -> Result<(), TreesError> {
        assert!(self.at <= self.bytes.len());
        if self.bytes.len() - self.at < count {
            return Err(TreesError::Truncated {
                bytes: self.bytes.len(),
            });
        }
        Ok(())
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], TreesError> {
        self.require(N)?;
        let mut taken = [0u8; N];
        taken.copy_from_slice(&self.bytes[self.at..self.at + N]);
        self.at += N;
        Ok(taken)
    }

    fn u32(&mut self) -> Result<u32, TreesError> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn f64(&mut self) -> Result<f64, TreesError> {
        Ok(f64::from_le_bytes(self.take()?))
    }

    fn node(&mut self, tree: usize, node: usize) -> Result<Node, TreesError> {
        let feature = self.u32()?;
        let is_leaf = match self.u32()? {
            0 => false,
            1 => true,
            _ => return Err(TreesError::LeafFlagInvalid { tree, node }),
        };
        Ok(Node {
            feature,
            is_leaf,
            threshold: self.f64()?,
            value: self.f64()?,
            left: self.u32()?,
            right: self.u32()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(value: f64) -> Node {
        Node {
            feature: 0,
            is_leaf: true,
            threshold: 0.0,
            value,
            left: 0,
            right: 0,
        }
    }

    fn split(feature: u32, threshold: f64, left: u32, right: u32) -> Node {
        Node {
            feature,
            is_leaf: false,
            threshold,
            value: 0.0,
            left,
            right,
        }
    }

    /// The format, written by hand, so the tests don't lean on `to_bytes`.
    fn encode(base: f64, trees: &[Vec<Node>]) -> Vec<u8> {
        let mut bytes = (trees.len() as u32).to_le_bytes().to_vec();
        bytes.extend(base.to_le_bytes());
        for tree in trees {
            bytes.extend((tree.len() as u32).to_le_bytes());
            for node in tree {
                bytes.extend(node.feature.to_le_bytes());
                bytes.extend(u32::from(node.is_leaf).to_le_bytes());
                bytes.extend(node.threshold.to_le_bytes());
                bytes.extend(node.value.to_le_bytes());
                bytes.extend(node.left.to_le_bytes());
                bytes.extend(node.right.to_le_bytes());
            }
        }
        bytes
    }

    /// Two trees over two features: `x0 <= 0.5` then `x1 <= 2`, and a stump
    /// on `x1 <= -1`.
    fn sample() -> Vec<u8> {
        encode(
            10.0,
            &[
                vec![
                    split(0, 0.5, 1, 2),
                    leaf(1.0),
                    split(1, 2.0, 3, 4),
                    leaf(2.0),
                    leaf(3.0),
                ],
                vec![split(1, -1.0, 2, 1), leaf(0.25), leaf(-0.25)],
            ],
        )
    }

    fn one_tree(nodes: Vec<Node>) -> Result<Trees, TreesError> {
        Trees::from_bytes(&encode(0.0, &[nodes]))
    }

    #[test]
    fn predicts_the_base_plus_a_leaf_a_tree() {
        let trees = Trees::from_bytes(&sample()).unwrap();
        assert_eq!(trees.tree_count(), 2);
        assert_eq!(trees.node_count(), 8);
        assert_eq!(trees.feature_count(), 2);
        assert_eq!(trees.base(), 10.0);
        assert_eq!(trees.predict(&[0.0, 0.0]).unwrap(), 11.25);
        assert_eq!(trees.predict(&[1.0, 0.0]).unwrap(), 12.25);
        assert_eq!(trees.predict(&[1.0, 5.0]).unwrap(), 13.25);
        assert_eq!(trees.predict(&[0.0, -3.0]).unwrap(), 10.75);
    }

    #[test]
    fn a_feature_equal_to_the_threshold_goes_left() {
        let trees = Trees::from_bytes(&sample()).unwrap();
        assert_eq!(trees.predict(&[0.5, 0.0]).unwrap(), 11.25);
        assert_eq!(trees.predict(&[1.0, 2.0]).unwrap(), 12.25);
        assert_eq!(trees.predict(&[0.0, -1.0]).unwrap(), 10.75);
    }

    #[test]
    fn a_feature_is_rounded_to_f32_before_the_comparison() {
        // 0.1 as f32 is a little above 0.1, so against a threshold of 0.1
        // the feature 0.1 goes right, as in scikit-learn's classic trees.
        let trees = one_tree(vec![split(0, 0.1, 1, 2), leaf(-1.0), leaf(1.0)]).unwrap();
        assert!(f64::from(0.1_f32) > 0.1);
        assert_eq!(trees.predict(&[0.1]).unwrap(), 1.0);
        assert_eq!(trees.predict(&[0.099_999_99]).unwrap(), -1.0);
    }

    #[test]
    fn infinite_features_follow_the_comparison() {
        let trees = Trees::from_bytes(&sample()).unwrap();
        assert_eq!(
            trees.predict(&[f64::NEG_INFINITY, f64::INFINITY]).unwrap(),
            11.25
        );
        // Past f32's range the rounding is to infinity: still to the right.
        assert_eq!(trees.predict(&[1e300, 1e300]).unwrap(), 13.25);
    }

    #[test]
    fn predict_refuses_too_few_features_and_nan() {
        let trees = Trees::from_bytes(&sample()).unwrap();
        assert_eq!(
            trees.predict(&[0.0]).unwrap_err(),
            TreesError::FeaturesTooShort {
                given: 1,
                needed: 2
            }
        );
        assert_eq!(
            trees.predict(&[0.0, f64::NAN]).unwrap_err(),
            TreesError::FeatureNotANumber { feature: 1 }
        );
        // Features past those the model reads are ignored, NaN or not.
        assert_eq!(trees.predict(&[0.0, 0.0, f64::NAN]).unwrap(), 11.25);
    }

    #[test]
    fn a_model_of_no_trees_or_only_leaves_predicts_from_nothing() {
        let empty = Trees::from_bytes(&encode(1.5, &[])).unwrap();
        assert_eq!(empty.feature_count(), 0);
        assert_eq!(empty.predict(&[]).unwrap(), 1.5);
        let stump = Trees::from_bytes(&encode(1.5, &[vec![leaf(2.0)]])).unwrap();
        assert_eq!(stump.feature_count(), 0);
        assert_eq!(stump.predict(&[]).unwrap(), 3.5);
    }

    #[test]
    fn bytes_round_trip() {
        let bytes = sample();
        let trees = Trees::from_bytes(&bytes).unwrap();
        assert_eq!(trees.to_bytes(), bytes);
        assert_eq!(Trees::from_bytes(&trees.to_bytes()).unwrap(), trees);
    }

    #[test]
    fn every_truncation_is_refused() {
        let bytes = sample();
        for len in 0..bytes.len() {
            let error = Trees::from_bytes(&bytes[..len]).unwrap_err();
            assert_eq!(error, TreesError::Truncated { bytes: len }, "at {len}");
        }
    }

    #[test]
    fn trailing_and_oversized_input_is_refused() {
        let mut bytes = sample();
        bytes.push(0);
        assert_eq!(
            Trees::from_bytes(&bytes).unwrap_err(),
            TreesError::TrailingBytes { extra: 1 }
        );
        let long = vec![0u8; BYTES_MAX + 1];
        assert_eq!(
            Trees::from_bytes(&long).unwrap_err(),
            TreesError::TooLong {
                bytes: BYTES_MAX + 1
            }
        );
    }

    #[test]
    fn counts_over_the_limits_are_refused_before_anything_is_reserved() {
        let mut bytes = ((TREE_COUNT_MAX + 1) as u32).to_le_bytes().to_vec();
        bytes.extend(0.0_f64.to_le_bytes());
        assert_eq!(
            Trees::from_bytes(&bytes).unwrap_err(),
            TreesError::TooManyTrees {
                trees: TREE_COUNT_MAX + 1
            }
        );
        // u32::MAX trees, as a corrupted header would say.
        bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            Trees::from_bytes(&bytes).unwrap_err(),
            TreesError::TooManyTrees { .. }
        ));

        for nodes in [0, TREE_NODE_COUNT_MAX + 1, u32::MAX as usize] {
            let mut bytes = 1u32.to_le_bytes().to_vec();
            bytes.extend(0.0_f64.to_le_bytes());
            bytes.extend((nodes as u32).to_le_bytes());
            assert_eq!(
                Trees::from_bytes(&bytes).unwrap_err(),
                TreesError::NodeCountOutOfRange { tree: 0, nodes }
            );
        }
        // A count within the limits that the input cannot back.
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.extend(0.0_f64.to_le_bytes());
        bytes.extend((TREE_NODE_COUNT_MAX as u32).to_le_bytes());
        assert!(matches!(
            Trees::from_bytes(&bytes).unwrap_err(),
            TreesError::Truncated { .. }
        ));
    }

    /// A complete binary tree of `node_count` nodes, an odd number.
    fn complete_tree(node_count: usize) -> Vec<Node> {
        assert_eq!(node_count % 2, 1);
        let mut nodes = Vec::with_capacity(node_count);
        for node in 0..node_count / 2 {
            let (left, right) = (2 * node as u32 + 1, 2 * node as u32 + 2);
            nodes.push(split(0, 0.0, left, right));
        }
        nodes.resize(node_count, leaf(0.0));
        nodes
    }

    #[test]
    fn the_total_node_limit_is_enforced() {
        // Sixteen trees at the per-tree limit leave room for 16 nodes: a
        // tree of 15 fits, and a tree of 17 is refused by its count.
        let trees_at_limit = NODE_COUNT_MAX / TREE_NODE_COUNT_MAX;
        let room = NODE_COUNT_MAX - trees_at_limit * TREE_NODE_COUNT_MAX;
        assert_eq!(room, 16);
        let mut all = vec![complete_tree(TREE_NODE_COUNT_MAX); trees_at_limit];
        all.push(complete_tree(room - 1));
        let trees = Trees::from_bytes(&encode(0.0, &all)).unwrap();
        assert_eq!(trees.node_count(), NODE_COUNT_MAX - 1);
        assert_eq!(trees.predict(&[0.0]).unwrap(), 0.0);
        all.pop();
        all.push(complete_tree(room + 1));
        assert_eq!(
            Trees::from_bytes(&encode(0.0, &all)).unwrap_err(),
            TreesError::NodeCountOutOfRange {
                tree: trees_at_limit,
                nodes: room + 1
            }
        );
    }

    #[test]
    fn numbers_that_are_not_finite_are_refused() {
        assert_eq!(
            Trees::from_bytes(&encode(f64::NAN, &[])).unwrap_err(),
            TreesError::BaseNotFinite
        );
        assert_eq!(
            one_tree(vec![leaf(f64::INFINITY)]).unwrap_err(),
            TreesError::NumberNotFinite { tree: 0, node: 0 }
        );
        assert_eq!(
            one_tree(vec![split(0, f64::NAN, 1, 2), leaf(0.0), leaf(0.0)]).unwrap_err(),
            TreesError::NumberNotFinite { tree: 0, node: 0 }
        );
    }

    #[test]
    fn a_leaf_flag_other_than_zero_or_one_is_refused() {
        let mut bytes = encode(0.0, &[vec![leaf(1.0)]]);
        // The flag is the node's second u32.
        let flag_at = HEADER_BYTES + TREE_HEADER_BYTES + 4;
        bytes[flag_at] = 2;
        assert_eq!(
            Trees::from_bytes(&bytes).unwrap_err(),
            TreesError::LeafFlagInvalid { tree: 0, node: 0 }
        );
    }

    #[test]
    fn children_out_of_range_are_refused() {
        for (left, right) in [(1, 3), (3, 1), (u32::MAX, 1), (1, 1)] {
            let error = one_tree(vec![split(0, 0.0, left, right), leaf(0.0), leaf(0.0)]);
            assert_eq!(
                error.unwrap_err(),
                TreesError::ChildInvalid { tree: 0, node: 0 },
                "children {left} and {right}"
            );
        }
    }

    #[test]
    fn cycles_and_shared_nodes_are_refused() {
        // A split that is its own child: an endless walk, were it loaded.
        assert_eq!(
            one_tree(vec![split(0, 0.0, 0, 1), leaf(0.0)]).unwrap_err(),
            TreesError::NodeReachedTwice { tree: 0, node: 0 }
        );
        // A longer cycle: 0 -> 1 -> 0.
        assert_eq!(
            one_tree(vec![
                split(0, 0.0, 1, 2),
                split(0, 0.0, 0, 3),
                leaf(0.0),
                leaf(0.0)
            ])
            .unwrap_err(),
            TreesError::NodeReachedTwice { tree: 0, node: 0 }
        );
        // Two parents for node 3: no cycle, and still not a tree.
        let shared = vec![
            split(0, 0.0, 1, 2),
            split(0, 0.0, 3, 4),
            split(0, 0.0, 3, 5),
            leaf(0.0),
            leaf(0.0),
            leaf(0.0),
        ];
        assert_eq!(
            one_tree(shared).unwrap_err(),
            TreesError::NodeReachedTwice { tree: 0, node: 3 }
        );
    }

    #[test]
    fn unreachable_nodes_are_refused() {
        assert_eq!(
            one_tree(vec![leaf(0.0), leaf(1.0)]).unwrap_err(),
            TreesError::NodeUnreachable { tree: 0, node: 1 }
        );
    }

    #[test]
    fn a_feature_index_over_the_limit_is_refused() {
        let at_limit = FEATURE_COUNT_MAX as u32;
        assert_eq!(
            one_tree(vec![split(at_limit, 0.0, 1, 2), leaf(0.0), leaf(0.0)]).unwrap_err(),
            TreesError::FeatureOutOfRange { tree: 0, node: 0 }
        );
        let trees = one_tree(vec![split(at_limit - 1, 0.0, 1, 2), leaf(0.0), leaf(0.0)]).unwrap();
        assert_eq!(trees.feature_count(), FEATURE_COUNT_MAX);
    }

    #[test]
    fn the_error_names_the_tree_and_node() {
        let bytes = encode(
            0.0,
            &[
                vec![leaf(0.0)],
                vec![split(0, 0.0, 1, 2), leaf(0.0), leaf(f64::NAN)],
            ],
        );
        let error = Trees::from_bytes(&bytes).unwrap_err();
        assert_eq!(error, TreesError::NumberNotFinite { tree: 1, node: 2 });
        assert_eq!(
            error.to_string(),
            "tree 1, node 2: a threshold or leaf value is not finite"
        );
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // A fixed-seed generator: corrupt a valid model one byte at a time,
        // and load noise. Whatever loads must also predict without a panic.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let valid = sample();
        for _ in 0..20_000 {
            let mut bytes = valid.clone();
            let at = (next() % bytes.len() as u64) as usize;
            bytes[at] = next() as u8;
            if let Ok(trees) = Trees::from_bytes(&bytes) {
                let features = vec![0.0; trees.feature_count()];
                trees.predict(&features).unwrap();
            }
        }
        for _ in 0..2_000 {
            let len = (next() % 200) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if let Ok(trees) = Trees::from_bytes(&bytes) {
                let features = vec![0.0; trees.feature_count()];
                trees.predict(&features).unwrap();
            }
        }
    }
}
