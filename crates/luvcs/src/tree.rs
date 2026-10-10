//! `tree.patricia128.v1`: the immutable snapshot map from `ObjectId` to
//! `ObjectStateId`.
//!
//! The map is a canonical binary Patricia tree over the 128-bit object ID.
//! The same map always has the same root regardless of insertion order, and
//! only the nodes on a changed path are new. Nodes are content-addressed
//! (`TreeId`) so they can be shared between commits and verified when read.
//!
//! Node invariants enforced on decode and on [`verify`]:
//! - no unary branches, both children present;
//! - a branch's `prefix` holds exactly `prefix_len` bits, unused bits zero;
//! - every key under a child starts with the branch prefix and has the
//!   child's bit (0 = left, 1 = right) at position `prefix_len`;
//! - no cycles, no duplicate keys, no mismatched child IDs.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::cbor::{self, Value};
use crate::digest::{Digest, DigestError};
use crate::id::{Domain, IdKey, ObjectId, ObjectStateId, TreeId, keyed_id};
use crate::model::{CoreError, TREE_FORMAT_V1};

const KEY_BITS: u16 = 128;

/// One node of the tree, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Empty,
    Leaf {
        key: ObjectId,
        state: ObjectStateId,
    },
    Branch {
        /// Number of leading bits shared by every key below this node.
        prefix_len: u16,
        /// The shared prefix, unused bits zero.
        prefix: [u8; 16],
        left: TreeId,
        right: TreeId,
    },
}

fn bit(key: &[u8; 16], i: u16) -> bool {
    (key[(i / 8) as usize] >> (7 - (i % 8))) & 1 == 1
}

fn mask_prefix(key: &[u8; 16], len: u16) -> [u8; 16] {
    let mut out = [0u8; 16];
    let full = (len / 8) as usize;
    out[..full].copy_from_slice(&key[..full]);
    let rem = len % 8;
    if rem > 0 && full < 16 {
        out[full] = key[full] & (0xffu8 << (8 - rem));
    }
    out
}

fn common_prefix_len(a: &[u8; 16], b: &[u8; 16]) -> u16 {
    let mut n = 0u16;
    for i in 0..16 {
        let x = a[i] ^ b[i];
        if x == 0 {
            n += 8;
        } else {
            n += x.leading_zeros() as u16;
            break;
        }
    }
    n
}

impl Node {
    pub fn to_cbor(&self) -> Value {
        match self {
            Node::Empty => Value::map([("kind", Value::text("empty"))]),
            Node::Leaf { key, state } => Value::map([
                ("kind", Value::text("leaf")),
                ("key", key.to_cbor()),
                ("state", state.to_cbor()),
            ]),
            Node::Branch {
                prefix_len,
                prefix,
                left,
                right,
            } => Value::map([
                ("kind", Value::text("branch")),
                ("prefix_len", Value::Uint(u64::from(*prefix_len))),
                ("prefix", Value::Bytes(prefix.to_vec())),
                ("left", left.to_cbor()),
                ("right", right.to_cbor()),
            ]),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        cbor::encode(&self.to_cbor())
    }

    pub fn decode(bytes: &[u8]) -> Result<Node, CoreError> {
        Self::from_cbor(&cbor::decode(bytes)?)
    }

    pub fn from_cbor(value: &Value) -> Result<Node, CoreError> {
        const KIND: &str = "tree.node";
        let invalid = |d: &str| CoreError::Invalid {
            kind: KIND,
            detail: d.to_owned(),
        };
        let entries = value.map_entries().ok_or_else(|| invalid("not a map"))?;
        let kind = entries
            .get("kind")
            .and_then(|v| v.as_text())
            .ok_or_else(|| invalid("missing kind"))?;
        let expect_keys = |keys: &[&str]| -> Result<(), CoreError> {
            if entries.len() != keys.len() || !keys.iter().all(|k| entries.contains_key(k)) {
                return Err(invalid("unexpected fields"));
            }
            Ok(())
        };
        match kind {
            "empty" => {
                expect_keys(&["kind"])?;
                Ok(Node::Empty)
            }
            "leaf" => {
                expect_keys(&["kind", "key", "state"])?;
                Ok(Node::Leaf {
                    key: ObjectId::from_cbor(entries["key"]).ok_or_else(|| invalid("bad key"))?,
                    state: ObjectStateId::from_cbor(entries["state"])
                        .ok_or_else(|| invalid("bad state"))?,
                })
            }
            "branch" => {
                expect_keys(&["kind", "prefix_len", "prefix", "left", "right"])?;
                let prefix_len = entries["prefix_len"]
                    .as_u64()
                    .and_then(|v| u16::try_from(v).ok())
                    .filter(|v| *v < KEY_BITS)
                    .ok_or_else(|| invalid("bad prefix_len"))?;
                let prefix: [u8; 16] = entries["prefix"]
                    .as_bytes()
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| invalid("bad prefix"))?;
                if mask_prefix(&prefix, prefix_len) != prefix {
                    return Err(invalid("prefix has unused bits set"));
                }
                let left = TreeId::from_cbor(entries["left"]).ok_or_else(|| invalid("bad left"))?;
                let right =
                    TreeId::from_cbor(entries["right"]).ok_or_else(|| invalid("bad right"))?;
                if left == right {
                    return Err(invalid("left and right are the same node"));
                }
                Ok(Node::Branch {
                    prefix_len,
                    prefix,
                    left,
                    right,
                })
            }
            _ => Err(CoreError::Unsupported {
                kind: KIND,
                format: TREE_FORMAT_V1,
                missing_capabilities: vec![format!("tree.node.{kind}")],
            }),
        }
    }

    pub fn id<D: Digest>(&self, digest: &D, key: &IdKey) -> Result<TreeId, DigestError> {
        Ok(TreeId(keyed_id(
            digest,
            key,
            Domain::Tree,
            TREE_FORMAT_V1,
            self.to_cbor(),
        )?))
    }
}

/// The result of building a tree: its root and every node, by ID.
#[derive(Debug, Clone)]
pub struct BuiltTree {
    pub root: TreeId,
    pub nodes: BTreeMap<TreeId, Node>,
}

/// Builds the canonical tree of `entries`, returning every node.
///
/// Node sharing with a previously built tree happens naturally: identical
/// subtrees get identical IDs, so a store that already has a node keeps it.
pub fn build<D: Digest>(
    digest: &D,
    key: &IdKey,
    entries: &BTreeMap<ObjectId, ObjectStateId>,
) -> Result<BuiltTree, DigestError> {
    let mut nodes = BTreeMap::new();
    let items: Vec<(&ObjectId, &ObjectStateId)> = entries.iter().collect();
    let root = build_range(digest, key, &items, 0, &mut nodes)?;
    Ok(BuiltTree { root, nodes })
}

fn build_range<D: Digest>(
    digest: &D,
    key: &IdKey,
    items: &[(&ObjectId, &ObjectStateId)],
    depth: u16,
    nodes: &mut BTreeMap<TreeId, Node>,
) -> Result<TreeId, DigestError> {
    let node = match items {
        [] => Node::Empty,
        [(k, s)] => Node::Leaf {
            key: **k,
            state: **s,
        },
        _ => {
            // Common prefix of the first and last key is the common prefix of
            // the whole sorted range.
            let first = &items[0].0.0;
            let last = &items[items.len() - 1].0.0;
            let prefix_len = common_prefix_len(first, last).max(depth);
            debug_assert!(prefix_len < KEY_BITS, "duplicate keys cannot reach here");
            let split = items.partition_point(|(k, _)| !bit(&k.0, prefix_len));
            let left = build_range(digest, key, &items[..split], prefix_len + 1, nodes)?;
            let right = build_range(digest, key, &items[split..], prefix_len + 1, nodes)?;
            Node::Branch {
                prefix_len,
                prefix: mask_prefix(first, prefix_len),
                left,
                right,
            }
        }
    };
    let id = node.id(digest, key)?;
    nodes.insert(id, node);
    Ok(id)
}

/// The empty tree's ID under `key`.
pub fn empty_root<D: Digest>(digest: &D, key: &IdKey) -> Result<TreeId, DigestError> {
    Node::Empty.id(digest, key)
}

/// A walk visits at most this many nodes.
pub const MAX_WALK_NODES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    #[error("node {0} is missing")]
    Missing(TreeId),
    #[error("node {0} does not hash to its id")]
    IdMismatch(TreeId),
    #[error("key {key} is not under the prefix of node {node}")]
    PrefixMismatch { node: TreeId, key: ObjectId },
    #[error("node {0} is reachable twice (cycle or shared leaf)")]
    Cycle(TreeId),
    #[error("duplicate key {0}")]
    DuplicateKey(ObjectId),
    #[error("empty node below a branch at {0}")]
    EmptyBelowBranch(TreeId),
    #[error("walk exceeded {0} nodes")]
    TooManyNodes(usize),
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("{0}")]
    Digest(#[from] DigestError),
}

/// Walks a tree from `root`, verifying every invariant, and returns its
/// entries. `fetch` returns the stored node bytes for an ID.
pub fn walk_verified<D, F>(
    digest: &D,
    key: &IdKey,
    root: TreeId,
    mut fetch: F,
) -> Result<BTreeMap<ObjectId, ObjectStateId>, TreeError>
where
    D: Digest,
    F: FnMut(&TreeId) -> Option<Vec<u8>>,
{
    let mut out = BTreeMap::new();
    let mut seen: BTreeSet<TreeId> = BTreeSet::new();
    // (node id, required prefix bits, required prefix, must be non-empty)
    let mut stack: Vec<(TreeId, u16, [u8; 16], bool)> = vec![(root, 0, [0; 16], false)];
    let mut visited = 0usize;
    while let Some((id, req_len, req_prefix, below_branch)) = stack.pop() {
        visited += 1;
        if visited > MAX_WALK_NODES {
            return Err(TreeError::TooManyNodes(MAX_WALK_NODES));
        }
        if !seen.insert(id) {
            return Err(TreeError::Cycle(id));
        }
        let bytes = fetch(&id).ok_or(TreeError::Missing(id))?;
        let node = Node::decode(&bytes)?;
        if node.id(digest, key)? != id {
            return Err(TreeError::IdMismatch(id));
        }
        match node {
            Node::Empty => {
                if below_branch {
                    return Err(TreeError::EmptyBelowBranch(id));
                }
            }
            Node::Leaf { key: k, state } => {
                if mask_prefix(&k.0, req_len) != req_prefix {
                    return Err(TreeError::PrefixMismatch { node: id, key: k });
                }
                if out.insert(k, state).is_some() {
                    return Err(TreeError::DuplicateKey(k));
                }
            }
            Node::Branch {
                prefix_len,
                prefix,
                left,
                right,
            } => {
                if prefix_len < req_len || mask_prefix(&prefix, req_len) != req_prefix {
                    return Err(TreeError::PrefixMismatch {
                        node: id,
                        key: ObjectId(prefix),
                    });
                }
                let mut right_prefix = prefix;
                right_prefix[(prefix_len / 8) as usize] |= 0x80 >> (prefix_len % 8);
                stack.push((right, prefix_len + 1, right_prefix, true));
                stack.push((left, prefix_len + 1, prefix, true));
            }
        }
    }
    Ok(out)
}

/// Difference between two entry maps: what `after` adds, removes or changes
/// relative to `before`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeDiff {
    pub added: Vec<(ObjectId, ObjectStateId)>,
    pub removed: Vec<(ObjectId, ObjectStateId)>,
    pub changed: Vec<(ObjectId, ObjectStateId, ObjectStateId)>,
}

pub fn diff(
    before: &BTreeMap<ObjectId, ObjectStateId>,
    after: &BTreeMap<ObjectId, ObjectStateId>,
) -> TreeDiff {
    let mut d = TreeDiff::default();
    for (k, a) in after {
        match before.get(k) {
            None => d.added.push((*k, *a)),
            Some(b) if b != a => d.changed.push((*k, *b, *a)),
            _ => {}
        }
    }
    for (k, b) in before {
        if !after.contains_key(k) {
            d.removed.push((*k, *b));
        }
    }
    d
}

/// In-memory node store for tests and small trees.
pub type NodeBytes = HashMap<TreeId, Vec<u8>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::RustDigest;

    fn key() -> IdKey {
        IdKey([3; 32])
    }

    fn entries(n: u8) -> BTreeMap<ObjectId, ObjectStateId> {
        (0..n)
            .map(|i| {
                let mut k = [0u8; 16];
                k[0] = i.wrapping_mul(37);
                k[15] = i;
                (ObjectId(k), ObjectStateId([i; 32]))
            })
            .collect()
    }

    fn store_of(t: &BuiltTree) -> NodeBytes {
        t.nodes.iter().map(|(id, n)| (*id, n.encode())).collect()
    }

    #[test]
    fn root_is_independent_of_insertion_order_and_round_trips() {
        let e = entries(40);
        let t1 = build(&RustDigest, &key(), &e).unwrap();
        let mut rev: Vec<_> = e.iter().collect();
        rev.reverse();
        let e2: BTreeMap<_, _> = rev.into_iter().map(|(k, v)| (*k, *v)).collect();
        let t2 = build(&RustDigest, &key(), &e2).unwrap();
        assert_eq!(t1.root, t2.root);
        let store = store_of(&t1);
        let back =
            walk_verified(&RustDigest, &key(), t1.root, |id| store.get(id).cloned()).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn single_change_rebuilds_only_the_path() {
        let e = entries(64);
        let t1 = build(&RustDigest, &key(), &e).unwrap();
        let mut e2 = e.clone();
        let k = *e2.keys().next().unwrap();
        e2.insert(k, ObjectStateId([200; 32]));
        let t2 = build(&RustDigest, &key(), &e2).unwrap();
        let new_nodes = t2
            .nodes
            .keys()
            .filter(|id| !t1.nodes.contains_key(id))
            .count();
        assert!(new_nodes <= 8, "{new_nodes} new nodes for one change");
        let d = diff(&e, &e2);
        assert_eq!(d.changed.len(), 1);
        assert!(d.added.is_empty() && d.removed.is_empty());
    }

    #[test]
    fn empty_tree_has_a_stable_root() {
        let t = build(&RustDigest, &key(), &BTreeMap::new()).unwrap();
        assert_eq!(t.root, empty_root(&RustDigest, &key()).unwrap());
        let store = store_of(&t);
        assert!(
            walk_verified(&RustDigest, &key(), t.root, |id| store.get(id).cloned())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn verification_rejects_tampered_nodes() {
        let e = entries(10);
        let t = build(&RustDigest, &key(), &e).unwrap();
        let mut store = store_of(&t);
        // Swap a leaf's state bytes: ID no longer matches.
        let (leaf_id, _) = t
            .nodes
            .iter()
            .find(|(_, n)| matches!(n, Node::Leaf { .. }))
            .unwrap();
        let mut bytes = store[leaf_id].clone();
        let n = bytes.len();
        bytes[n - 1] ^= 1;
        store.insert(*leaf_id, bytes);
        assert_eq!(
            walk_verified(&RustDigest, &key(), t.root, |id| store.get(id).cloned()),
            Err(TreeError::IdMismatch(*leaf_id))
        );
        // Missing node.
        let mut store = store_of(&t);
        store.remove(leaf_id);
        assert_eq!(
            walk_verified(&RustDigest, &key(), t.root, |id| store.get(id).cloned()),
            Err(TreeError::Missing(*leaf_id))
        );
    }

    #[test]
    fn decode_rejects_bad_branches() {
        let bad = Node::Branch {
            prefix_len: 4,
            prefix: [0xff; 16],
            left: TreeId([1; 32]),
            right: TreeId([2; 32]),
        };
        assert!(Node::decode(&bad.encode()).is_err());
        let same = Node::Branch {
            prefix_len: 4,
            prefix: mask_prefix(&[0xff; 16], 4),
            left: TreeId([1; 32]),
            right: TreeId([1; 32]),
        };
        assert!(Node::decode(&same.encode()).is_err());
    }
}
