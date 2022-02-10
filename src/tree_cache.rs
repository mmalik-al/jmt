// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! A transaction can have multiple operations on state. For example, it might update values
//! for a few existing keys. Imagine that we have the following tree.
//!
//! ```text
//!                 root0
//!                 /    \
//!                /      \
//!  key1 => value11        key2 => value21
//! ```
//!
//! The next transaction updates `key1`'s value to `value12` and `key2`'s value to `value22`.
//! Let's assume we update key2 first. Then the tree becomes:
//!
//! ```text
//!                   (on disk)              (in memory)
//!                     root0                  root1'
//!                    /     \                /     \
//!                   /   ___ \ _____________/       \
//!                  /  _/     \                      \
//!                 / _/        \                      \
//!                / /           \                      \
//!   key1 => value11           key2 => value21       key2 => value22
//!      (on disk)                 (on disk)            (in memory)
//! ```
//!
//! Note that
//!   1) we created a new version of the tree with `root1'` and the new `key2` node generated;
//!   2) both `root1'` and the new `key2` node are still held in memory within a batch of nodes
//!      that will be written into db atomically.
//!
//! Next, we need to update `key1`'s value. This time we are dealing with the tree starting from
//! the new root. Part of the tree is in memory and the rest of it is in database. We'll update the
//! left child and the new root. We should
//!   1) create a new version for `key1` child.
//!   2) update `root1'` directly instead of making another version.
//! The resulting tree should look like:
//!
//! ```text
//!                   (on disk)                                     (in memory)
//!                     root0                                         root1''
//!                    /     \                                       /     \
//!                   /       \                                     /       \
//!                  /         \                                   /         \
//!                 /           \                                 /           \
//!                /             \                               /             \
//!   key1 => value11             key2 => value21  key1 => value12              key2 => value22
//!      (on disk)                   (on disk)       (in memory)                  (in memory)
//! ```
//!
//! This means that we need to be able to tell whether to create a new version of a node or to
//! update an existing node by deleting it and creating a new node directly. `TreeCache` provides
//! APIs to cache intermediate nodes and values in memory and simplify the actual tree
//! implementation.
//!
//! If we are dealing with a single-version tree, any complex tree operation can be seen as a
//! collection of the following operations:
//!   - Put a new node.
//!   - Delete a node.
//! When we apply these operations on a multi-version tree:
//!   1) Put a new node.
//!   2) When we remove a node, if the node is in the previous on-disk version, we don't need to do
//!      anything. Otherwise we delete it from the tree cache.
//! Updating node could be operated as deletion of the node followed by insertion of the updated
//! node.

use std::collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{bail, Result};

use crate::{
    metrics::DIEM_JELLYFISH_STORAGE_READS,
    node_type::{Node, NodeKey},
    storage::{
        NodeBatch, NodeStats, StaleNodeIndex, StaleNodeIndexBatch, TreeReader, TreeUpdateBatch,
    },
    types::{Version, PRE_GENESIS_VERSION},
    RootHash,
};

/// `FrozenTreeCache` is used as a field of `TreeCache` storing all the nodes and values that
/// are generated by earlier transactions so they have to be immutable. The motivation of
/// `FrozenTreeCache` is to let `TreeCache` freeze intermediate results from each transaction to
/// help commit more than one transaction in a row atomically.
struct FrozenTreeCache {
    /// Immutable node_cache.
    node_cache: NodeBatch,

    /// Immutable stale_node_index_cache.
    stale_node_index_cache: StaleNodeIndexBatch,

    /// the stats vector including the number of new nodes, new leaves, stale nodes and stale leaves.
    node_stats: Vec<NodeStats>,

    /// Frozen root hashes after each earlier transaction.
    root_hashes: Vec<RootHash>,
}

impl FrozenTreeCache {
    fn new() -> Self {
        Self {
            node_cache: BTreeMap::new(),
            stale_node_index_cache: BTreeSet::new(),
            node_stats: Vec::new(),
            root_hashes: Vec::new(),
        }
    }
}

/// `TreeCache` is a in-memory cache for per-transaction updates of sparse Merkle nodes and values.
pub struct TreeCache<'a, R> {
    /// `NodeKey` of the current root node in cache.
    root_node_key: NodeKey,

    /// The version of the transaction to which the upcoming `put`s will be related.
    next_version: Version,

    /// Intermediate nodes keyed by node hash
    node_cache: HashMap<NodeKey, Node>,

    /// # of leaves in the `node_cache`,
    num_new_leaves: usize,

    /// Partial stale log. `NodeKey` to identify the stale record.
    stale_node_index_cache: HashSet<NodeKey>,

    /// # of leaves in the `stale_node_index_cache`,
    num_stale_leaves: usize,

    /// The immutable part of this cache, which will be committed to the underlying storage.
    frozen_cache: FrozenTreeCache,

    /// The underlying persistent storage.
    reader: &'a R,
}

impl<'a, R> TreeCache<'a, R>
where
    R: 'a + TreeReader,
{
    /// Constructs a new `TreeCache` instance.
    pub fn new(reader: &'a R, next_version: Version) -> Result<Self> {
        let mut node_cache = HashMap::new();
        let root_node_key = if next_version == 0 {
            let pre_genesis_root_key = NodeKey::new_empty_path(PRE_GENESIS_VERSION);
            let pre_genesis_root = reader.get_node_option(&pre_genesis_root_key)?;

            match pre_genesis_root {
                Some(_) => {
                    // This is to support the extreme case where things really went wild,
                    // and we need to ditch the transaction history and apply a new
                    // genesis on top of an existing state db.
                    pre_genesis_root_key
                }
                None => {
                    // Hack: We need to start from an empty tree, so we insert
                    // a null node beforehand deliberately to deal with this corner case.
                    let genesis_root_key = NodeKey::new_empty_path(0);
                    node_cache.insert(genesis_root_key.clone(), Node::new_null());
                    genesis_root_key
                }
            }
        } else {
            NodeKey::new_empty_path(next_version - 1)
        };
        Ok(Self {
            node_cache,
            stale_node_index_cache: HashSet::new(),
            frozen_cache: FrozenTreeCache::new(),
            root_node_key,
            next_version,
            reader,
            num_stale_leaves: 0,
            num_new_leaves: 0,
        })
    }

    /// Gets a node with given node key. If it doesn't exist in node cache, read from `reader`.
    pub fn get_node(&self, node_key: &NodeKey) -> Result<Node> {
        Ok(if let Some(node) = self.node_cache.get(node_key) {
            node.clone()
        } else if let Some(node) = self.frozen_cache.node_cache.get(node_key) {
            node.clone()
        } else {
            DIEM_JELLYFISH_STORAGE_READS.inc();
            self.reader.get_node(node_key)?
        })
    }

    /// Gets the current root node key.
    pub fn get_root_node_key(&self) -> &NodeKey {
        &self.root_node_key
    }

    /// Set roots `node_key`.
    pub fn set_root_node_key(&mut self, root_node_key: NodeKey) {
        self.root_node_key = root_node_key;
    }

    /// Puts the node with given hash as key into node_cache.
    pub fn put_node(&mut self, node_key: NodeKey, new_node: Node) -> Result<()> {
        match self.node_cache.entry(node_key) {
            Entry::Vacant(o) => {
                if new_node.is_leaf() {
                    self.num_new_leaves += 1
                }
                o.insert(new_node);
            }
            Entry::Occupied(o) => bail!("Node with key {:?} already exists in NodeBatch", o.key()),
        };
        Ok(())
    }

    /// Deletes a node with given hash.
    pub fn delete_node(&mut self, old_node_key: &NodeKey, is_leaf: bool) {
        // If node cache doesn't have this node, it means the node is in the previous version of
        // the tree on the disk.
        if self.node_cache.remove(old_node_key).is_none() {
            let is_new_entry = self.stale_node_index_cache.insert(old_node_key.clone());
            assert!(is_new_entry, "Node gets stale twice unexpectedly.");
            if is_leaf {
                self.num_stale_leaves += 1;
            }
        } else if is_leaf {
            self.num_new_leaves -= 1;
        }
    }

    /// Freezes all the contents in cache to be immutable and clear `node_cache`.
    pub fn freeze(&mut self) {
        let root_node_key = self.get_root_node_key();
        let root_hash = self
            .get_node(root_node_key)
            .unwrap_or_else(|_| unreachable!("Root node with key {:?} must exist", root_node_key))
            .hash();
        self.frozen_cache.root_hashes.push(RootHash(root_hash));
        let node_stats = NodeStats {
            new_nodes: self.node_cache.len(),
            new_leaves: self.num_new_leaves,
            stale_nodes: self.stale_node_index_cache.len(),
            stale_leaves: self.num_stale_leaves,
        };
        self.frozen_cache.node_stats.push(node_stats);
        self.frozen_cache.node_cache.extend(self.node_cache.drain());
        let stale_since_version = self.next_version;
        self.frozen_cache
            .stale_node_index_cache
            .extend(
                self.stale_node_index_cache
                    .drain()
                    .map(|node_key| StaleNodeIndex {
                        stale_since_version,
                        node_key,
                    }),
            );

        // Clean up
        self.num_stale_leaves = 0;
        self.num_new_leaves = 0;

        // Prepare for the next version after freezing
        self.next_version += 1;
    }
}

impl<'a, R> From<TreeCache<'a, R>> for (Vec<RootHash>, TreeUpdateBatch)
where
    R: 'a + TreeReader,
{
    fn from(tree_cache: TreeCache<'a, R>) -> Self {
        (
            tree_cache.frozen_cache.root_hashes,
            TreeUpdateBatch {
                node_batch: tree_cache.frozen_cache.node_cache,
                stale_node_index_batch: tree_cache.frozen_cache.stale_node_index_cache,
                node_stats: tree_cache.frozen_cache.node_stats,
            },
        )
    }
}
