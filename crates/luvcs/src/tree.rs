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

/// The tree at `root` with `changes` applied (`None` removes the key),
/// reading and writing only the nodes on the changed paths. The result is
/// the canonical tree of the new map, the same [`build`] gives; `nodes`
/// holds the nodes this created, which may include some the store has.
pub fn update<D, F>(
    digest: &D,
    key: &IdKey,
    root: TreeId,
    changes: &BTreeMap<ObjectId, Option<ObjectStateId>>,
    fetch: F,
) -> Result<BuiltTree, TreeError>
where
    D: Digest,
    F: FnMut(&TreeId) -> Option<Vec<u8>>,
{
    let mut updater = Updater {
        digest,
        key,
        fetch,
        nodes: BTreeMap::new(),
    };
    let changes: Vec<(ObjectId, Option<ObjectStateId>)> =
        changes.iter().map(|(k, v)| (*k, *v)).collect();
    let root = match updater.apply(Some(root), &changes, 0)? {
        Some(id) => id,
        None => updater.put(Node::Empty)?,
    };
    Ok(BuiltTree {
        root,
        nodes: updater.nodes,
    })
}

struct Updater<'a, D, F> {
    digest: &'a D,
    key: &'a IdKey,
    fetch: F,
    nodes: BTreeMap<TreeId, Node>,
}

impl<D, F> Updater<'_, D, F>
where
    D: Digest,
    F: FnMut(&TreeId) -> Option<Vec<u8>>,
{
    fn load(&mut self, id: TreeId) -> Result<Node, TreeError> {
        if let Some(node) = self.nodes.get(&id) {
            return Ok(node.clone());
        }
        let bytes = (self.fetch)(&id).ok_or(TreeError::Missing(id))?;
        let node = Node::decode(&bytes)?;
        if node.id(self.digest, self.key)? != id {
            return Err(TreeError::IdMismatch(id));
        }
        Ok(node)
    }

    fn put(&mut self, node: Node) -> Result<TreeId, TreeError> {
        let id = node.id(self.digest, self.key)?;
        self.nodes.insert(id, node);
        Ok(id)
    }

    /// The canonical subtree of `entries` (sorted, unique), or `None`.
    fn build(
        &mut self,
        entries: &[(ObjectId, ObjectStateId)],
        depth: u16,
    ) -> Result<Option<TreeId>, TreeError> {
        if entries.is_empty() {
            return Ok(None);
        }
        let items: Vec<(&ObjectId, &ObjectStateId)> = entries.iter().map(|(k, v)| (k, v)).collect();
        Ok(Some(build_range(
            self.digest,
            self.key,
            &items,
            depth,
            &mut self.nodes,
        )?))
    }

    /// Two subtrees split at bit `prefix_len` (left holds the 0 side).
    fn join(
        &mut self,
        prefix_len: u16,
        prefix: [u8; 16],
        left: Option<TreeId>,
        right: Option<TreeId>,
    ) -> Result<Option<TreeId>, TreeError> {
        Ok(match (left, right) {
            (Some(left), Some(right)) => Some(self.put(Node::Branch {
                prefix_len,
                prefix: mask_prefix(&prefix, prefix_len),
                left,
                right,
            })?),
            (one, None) | (None, one) => one,
        })
    }

    /// `node` (`None`: empty) with `changes` (sorted) applied. Every key in
    /// both agrees on the first `depth` bits.
    fn apply(
        &mut self,
        node: Option<TreeId>,
        changes: &[(ObjectId, Option<ObjectStateId>)],
        depth: u16,
    ) -> Result<Option<TreeId>, TreeError> {
        if changes.is_empty() {
            return Ok(node);
        }
        let inserts = |changes: &[(ObjectId, Option<ObjectStateId>)]| {
            changes
                .iter()
                .filter_map(|(k, v)| v.map(|v| (*k, v)))
                .collect::<Vec<_>>()
        };
        let Some(id) = node else {
            return self.build(&inserts(changes), depth);
        };
        match self.load(id)? {
            Node::Empty => self.build(&inserts(changes), depth),
            Node::Leaf { key, state } => {
                let mut map: BTreeMap<ObjectId, ObjectStateId> = BTreeMap::from([(key, state)]);
                for (k, v) in changes {
                    match v {
                        Some(v) => map.insert(*k, *v),
                        None => map.remove(k),
                    };
                }
                let entries: Vec<_> = map.into_iter().collect();
                self.build(&entries, depth)
            }
            Node::Branch {
                prefix_len,
                prefix,
                left,
                right,
            } => {
                // A removal outside the branch's prefix removes nothing.
                let changes: Vec<_> = changes
                    .iter()
                    .filter(|(k, v)| v.is_some() || mask_prefix(&k.0, prefix_len) == prefix)
                    .copied()
                    .collect();
                if changes.is_empty() {
                    return Ok(node);
                }
                // Where the first key leaves the branch's prefix, if any.
                let split = changes
                    .iter()
                    .map(|(k, _)| common_prefix_len(&k.0, &prefix).min(prefix_len))
                    .min()
                    .unwrap_or(prefix_len)
                    .max(depth);
                if split == prefix_len {
                    let mid = changes.partition_point(|(k, _)| !bit(&k.0, prefix_len));
                    let l = self.apply(Some(left), &changes[..mid], prefix_len + 1)?;
                    let r = self.apply(Some(right), &changes[mid..], prefix_len + 1)?;
                    return self.join(prefix_len, prefix, l, r);
                }
                // Some new keys leave the prefix at bit `split`: the branch
                // goes whole to its side of that bit, the others beside it.
                let side = bit(&prefix, split);
                let (with, apart): (Vec<_>, Vec<_>) = changes
                    .into_iter()
                    .partition(|(k, _)| bit(&k.0, split) == side);
                let kept = self.apply(node, &with, split + 1)?;
                let beside = self.build(&inserts(&apart), split + 1)?;
                if side {
                    self.join(split, prefix, beside, kept)
                } else {
                    self.join(split, prefix, kept, beside)
                }
            }
        }
    }
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

/// The state `object` has in the tree at `root`, reading only the nodes on
/// its path, each checked against its ID. `None` when the tree does not
/// hold it.
pub fn lookup<D, F>(
    digest: &D,
    key: &IdKey,
    root: TreeId,
    object: &ObjectId,
    mut fetch: F,
) -> Result<Option<ObjectStateId>, TreeError>
where
    D: Digest,
    F: FnMut(&TreeId) -> Option<Vec<u8>>,
{
    let mut id = root;
    // A path has at most one branch per key bit, then a leaf.
    for _ in 0..=KEY_BITS {
        let bytes = fetch(&id).ok_or(TreeError::Missing(id))?;
        let node = Node::decode(&bytes)?;
        if node.id(digest, key)? != id {
            return Err(TreeError::IdMismatch(id));
        }
        match node {
            Node::Empty => return Ok(None),
            Node::Leaf { key: k, state } => return Ok((k == *object).then_some(state)),
            Node::Branch {
                prefix_len,
                prefix,
                left,
                right,
            } => {
                if mask_prefix(&object.0, prefix_len) != prefix {
                    return Ok(None);
                }
                id = if bit(&object.0, prefix_len) {
                    right
                } else {
                    left
                };
            }
        }
    }
    Err(TreeError::Cycle(id))
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

/// The difference between the trees at `before` and `after`, reading only
/// where they differ: a subtree with the same ID at the same place is the
/// same on both sides and is skipped. Every node read is checked against
/// its ID. The lists are sorted by object ID.
pub fn diff_trees<D, F>(
    digest: &D,
    key: &IdKey,
    before: TreeId,
    after: TreeId,
    fetch: F,
) -> Result<TreeDiff, TreeError>
where
    D: Digest,
    F: FnMut(&TreeId) -> Option<Vec<u8>>,
{
    let mut differ = TreeDiffer {
        digest,
        key,
        fetch,
        out: TreeDiff::default(),
    };
    differ.nodes(before, after)?;
    let mut out = differ.out;
    out.added.sort();
    out.removed.sort();
    out.changed.sort();
    Ok(out)
}

struct TreeDiffer<'a, D, F> {
    digest: &'a D,
    key: &'a IdKey,
    fetch: F,
    out: TreeDiff,
}

impl<D, F> TreeDiffer<'_, D, F>
where
    D: Digest,
    F: FnMut(&TreeId) -> Option<Vec<u8>>,
{
    fn load(&mut self, id: TreeId) -> Result<Node, TreeError> {
        let bytes = (self.fetch)(&id).ok_or(TreeError::Missing(id))?;
        let node = Node::decode(&bytes)?;
        if node.id(self.digest, self.key)? != id {
            return Err(TreeError::IdMismatch(id));
        }
        Ok(node)
    }

    fn entries(&mut self, id: TreeId) -> Result<BTreeMap<ObjectId, ObjectStateId>, TreeError> {
        walk_verified(self.digest, self.key, id, &mut self.fetch)
    }

    fn removed(&mut self, id: TreeId) -> Result<(), TreeError> {
        let entries = self.entries(id)?;
        self.out.removed.extend(entries);
        Ok(())
    }

    fn added(&mut self, id: TreeId) -> Result<(), TreeError> {
        let entries = self.entries(id)?;
        self.out.added.extend(entries);
        Ok(())
    }

    /// A single entry on one side against a whole subtree on the other.
    fn leaf_against(
        &mut self,
        k: ObjectId,
        s: ObjectStateId,
        tree: TreeId,
        leaf_before: bool,
    ) -> Result<(), TreeError> {
        let mut entries = self.entries(tree)?;
        match entries.remove(&k) {
            Some(other) if other != s => self.out.changed.push(if leaf_before {
                (k, s, other)
            } else {
                (k, other, s)
            }),
            Some(_) => {}
            None if leaf_before => self.out.removed.push((k, s)),
            None => self.out.added.push((k, s)),
        }
        if leaf_before {
            self.out.added.extend(entries);
        } else {
            self.out.removed.extend(entries);
        }
        Ok(())
    }

    fn nodes(&mut self, a: TreeId, b: TreeId) -> Result<(), TreeError> {
        if a == b {
            return Ok(());
        }
        match (self.load(a)?, self.load(b)?) {
            (Node::Empty, _) => self.added(b),
            (_, Node::Empty) => self.removed(a),
            (Node::Leaf { key: ka, state: sa }, Node::Leaf { key: kb, state: sb }) => {
                if ka == kb {
                    self.out.changed.push((ka, sa, sb));
                } else {
                    self.out.removed.push((ka, sa));
                    self.out.added.push((kb, sb));
                }
                Ok(())
            }
            (Node::Leaf { key, state }, Node::Branch { .. }) => {
                self.leaf_against(key, state, b, true)
            }
            (Node::Branch { .. }, Node::Leaf { key, state }) => {
                self.leaf_against(key, state, a, false)
            }
            (
                Node::Branch {
                    prefix_len: la,
                    prefix: pa,
                    left: a_left,
                    right: a_right,
                },
                Node::Branch {
                    prefix_len: lb,
                    prefix: pb,
                    left: b_left,
                    right: b_right,
                },
            ) => {
                if la == lb && pa == pb {
                    self.nodes(a_left, b_left)?;
                    self.nodes(a_right, b_right)
                } else if la < lb && mask_prefix(&pb, la) == pa {
                    // Everything after holds lies under one child before.
                    if bit(&pb, la) {
                        self.removed(a_left)?;
                        self.nodes(a_right, b)
                    } else {
                        self.nodes(a_left, b)?;
                        self.removed(a_right)
                    }
                } else if lb < la && mask_prefix(&pa, lb) == pb {
                    if bit(&pa, lb) {
                        self.added(b_left)?;
                        self.nodes(a, b_right)
                    } else {
                        self.nodes(a, b_left)?;
                        self.added(b_right)
                    }
                } else {
                    // Disjoint key ranges.
                    self.removed(a)?;
                    self.added(b)
                }
            }
        }
    }
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

    /// Pseudo-random maps, to compare the walking diff with the map diff.
    fn random_entries(seed: u64, n: usize) -> BTreeMap<ObjectId, ObjectStateId> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        (0..n)
            .map(|_| {
                let mut k = [0u8; 16];
                k[..8].copy_from_slice(&next().to_le_bytes());
                // Few distinct high bytes, so trees share long prefixes.
                k[0] &= 0x0f;
                let mut st = [0u8; 32];
                st[..8].copy_from_slice(&next().to_le_bytes());
                (ObjectId(k), ObjectStateId(st))
            })
            .collect()
    }

    #[test]
    fn walking_diff_matches_the_map_diff() {
        for seed in 0..40u64 {
            let a = random_entries(seed, (seed as usize * 7) % 60);
            let mut b = a.clone();
            let changes = random_entries(seed + 1000, 1 + seed as usize % 9);
            for (i, (k, v)) in changes.into_iter().enumerate() {
                match i % 3 {
                    0 => {
                        b.insert(k, v);
                    }
                    1 => {
                        if let Some(first) = b.keys().nth(i).copied() {
                            b.remove(&first);
                        }
                    }
                    _ => {
                        if let Some(first) = b.keys().nth(i).copied() {
                            b.insert(first, v);
                        }
                    }
                }
            }
            let ta = build(&RustDigest, &key(), &a).unwrap();
            let tb = build(&RustDigest, &key(), &b).unwrap();
            let mut store = store_of(&ta);
            store.extend(store_of(&tb));
            let walked = diff_trees(&RustDigest, &key(), ta.root, tb.root, |id| {
                store.get(id).cloned()
            })
            .unwrap();
            assert_eq!(walked, diff(&a, &b), "seed {seed}");
            let back = diff_trees(&RustDigest, &key(), tb.root, ta.root, |id| {
                store.get(id).cloned()
            })
            .unwrap();
            assert_eq!(back, diff(&b, &a), "seed {seed} reversed");
        }
    }

    #[test]
    fn update_gives_the_tree_build_gives() {
        for seed in 0..60u64 {
            let a = random_entries(seed, (seed as usize * 11) % 80);
            let mut changes: BTreeMap<ObjectId, Option<ObjectStateId>> = BTreeMap::new();
            for (i, (k, v)) in random_entries(seed + 500, 1 + seed as usize % 12)
                .into_iter()
                .enumerate()
            {
                match i % 4 {
                    // New keys, removals of present and absent keys, edits.
                    0 => {
                        changes.insert(k, Some(v));
                    }
                    1 => {
                        if let Some(present) = a.keys().nth(i * 3) {
                            changes.insert(*present, None);
                        }
                    }
                    2 => {
                        changes.insert(k, None);
                    }
                    _ => {
                        if let Some(present) = a.keys().nth(i) {
                            changes.insert(*present, Some(v));
                        }
                    }
                }
            }
            // Sometimes everything goes.
            if seed % 13 == 0 {
                changes.extend(a.keys().map(|k| (*k, None)));
            }
            let mut b = a.clone();
            for (k, v) in &changes {
                match v {
                    Some(v) => b.insert(*k, *v),
                    None => b.remove(k),
                };
            }
            let ta = build(&RustDigest, &key(), &a).unwrap();
            let store = store_of(&ta);
            let updated = update(&RustDigest, &key(), ta.root, &changes, |id| {
                store.get(id).cloned()
            })
            .unwrap();
            let expected = build(&RustDigest, &key(), &b).unwrap();
            assert_eq!(updated.root, expected.root, "seed {seed}");
            // Every node the new tree needs is old or created.
            let mut all = store.clone();
            all.extend(store_of(&updated));
            let back = walk_verified(&RustDigest, &key(), updated.root, |id| all.get(id).cloned())
                .unwrap();
            assert_eq!(back, b, "seed {seed}");
        }
    }

    #[test]
    fn update_places_keys_that_leave_a_long_shared_prefix() {
        // Sequential IDs share their high bits; random ones leave them at
        // every depth.
        let seq = |i: u64| {
            let mut k = [0u8; 16];
            k[..8].copy_from_slice(&i.to_be_bytes());
            ObjectId(k)
        };
        for seed in 0..30u64 {
            let a: BTreeMap<_, _> = (1..=(20 + seed * 5))
                .map(|i| (seq(i), ObjectStateId([i as u8; 32])))
                .collect();
            let mut changes: BTreeMap<_, _> = random_entries(seed, 1 + seed as usize % 6)
                .into_iter()
                .map(|(k, v)| (k, Some(v)))
                .collect();
            changes.insert(seq(1000 + seed), Some(ObjectStateId([1; 32])));
            changes.insert(seq(3), None);
            let mut b = a.clone();
            for (k, v) in &changes {
                match v {
                    Some(v) => b.insert(*k, *v),
                    None => b.remove(k),
                };
            }
            let ta = build(&RustDigest, &key(), &a).unwrap();
            let store = store_of(&ta);
            let updated = update(&RustDigest, &key(), ta.root, &changes, |id| {
                store.get(id).cloned()
            })
            .unwrap();
            assert_eq!(
                updated.root,
                build(&RustDigest, &key(), &b).unwrap().root,
                "seed {seed}"
            );
        }
    }

    #[test]
    fn update_touches_only_the_changed_path() {
        let e = random_entries(7, 2000);
        let t = build(&RustDigest, &key(), &e).unwrap();
        let store = store_of(&t);
        let k = *e.keys().nth(1234).unwrap();
        let changes = BTreeMap::from([(k, Some(ObjectStateId([9; 32])))]);
        let mut reads = 0;
        let updated = update(&RustDigest, &key(), t.root, &changes, |id| {
            reads += 1;
            store.get(id).cloned()
        })
        .unwrap();
        assert!(reads < 40, "{reads} reads");
        assert!(updated.nodes.len() < 40, "{} nodes", updated.nodes.len());
    }

    #[test]
    fn lookup_reads_one_path() {
        let e = entries(50);
        let t = build(&RustDigest, &key(), &e).unwrap();
        let store = store_of(&t);
        let mut reads = 0;
        for (k, v) in &e {
            let found = lookup(&RustDigest, &key(), t.root, k, |id| {
                reads += 1;
                store.get(id).cloned()
            })
            .unwrap();
            assert_eq!(found, Some(*v));
        }
        assert!(reads < e.len() * 10, "{reads} reads");
        let absent = ObjectId([0xAB; 16]);
        assert_eq!(
            lookup(&RustDigest, &key(), t.root, &absent, |id| store
                .get(id)
                .cloned())
            .unwrap(),
            None
        );
        let empty = build(&RustDigest, &key(), &BTreeMap::new()).unwrap();
        let store = store_of(&empty);
        assert_eq!(
            lookup(&RustDigest, &key(), empty.root, &absent, |id| store
                .get(id)
                .cloned())
            .unwrap(),
            None
        );
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
