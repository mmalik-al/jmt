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

use alloc::{collections::BTreeSet, vec::Vec};
#[cfg(not(feature = "std"))]
use hashbrown::{hash_map::Entry, HashMap, HashSet};
#[cfg(feature = "std")]
use std::collections::{hash_map::Entry, HashMap, HashSet};

use crate::{
    node_type::{Node, NodeKey},
    storage::{
        NodeBatch, NodeStats, StaleNodeIndex, StaleNodeIndexBatch, TreeReader, TreeUpdateBatch,
    },
    types::{Version, PRE_GENESIS_VERSION},
    KeyHash, OwnedValue, RootHash, SimpleHasher,
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
            node_cache: Default::default(),
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

    /// Intermediate nodes keyed by node hash.
    node_cache: HashMap<NodeKey, Node>,

    /// Values keyed by version and keyhash.
    // TODO(@preston-evans98): Convert to a vector once we remove the non-batch APIs.
    // The Hashmap guarantees that if the same (version, key) pair is written several times, only the last
    // change is saved, which means that the TreeWriter can process node batches in parallel without racing.
    // The batch APIs already deduplicate operations on each key, so they don't need this HashMap.
    value_cache: HashMap<(Version, KeyHash), Option<OwnedValue>>,

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

/// An error returned when a [`Node`] could not be [put] into the [`TreeCache<'a, R>`].
///
/// [put]: TreeCache::put_node
#[derive(Debug, thiserror::Error)]
#[error("node with key {key:?} already exists")]
#[non_exhaustive]
pub struct NodeAlreadyExists {
    /// The key that already corresponded to a [`Node`] in the cache.
    key: NodeKey,
    /// The [`Node`] that could not be inserted into the cache.
    node: Node,
}

/// An error returned when [`TreeCache::freeze()`] fails.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FreezeError<E> {
    #[error(transparent)]
    GetNodeFailed(E),
    #[error(transparent)]
    PutNodeFailed(NodeAlreadyExists),
    #[error("root node could not be found")]
    RootNodeMissing,
}

impl<'a, R> TreeCache<'a, R>
where
    R: 'a + TreeReader,
    <R as TreeReader>::Error: std::error::Error + Send + Sync + 'static,
{
    /// Constructs a new `TreeCache` instance.
    pub fn new(reader: &'a R, next_version: Version) -> Result<Self, anyhow::Error> {
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
            value_cache: Default::default(),
        })
    }

    #[cfg(feature = "migration")]
    /// Instantiate a [`TreeCache`] over the a [`TreeReader`] that is defined
    /// against a root node key at version `current_version`.
    ///
    /// # Usage
    /// This method is used to perform incremental addition to a tree without
    /// increasing the tree version's number.
    pub fn new_overwrite(reader: &'a R, current_version: Version) -> Result<Self, anyhow::Error> {
        let node_cache = HashMap::new();
        let Some((node_key, _)) = reader.get_rightmost_leaf()? else {
            bail!("creating an overwrite cache for an empty tree is not supported")
        };

        anyhow::ensure!(
            node_key.version() == current_version,
            "the supplied version is not the latest version of the tree"
        );

        let root_node_key = NodeKey::new_empty_path(current_version);
        Ok(Self {
            node_cache,
            stale_node_index_cache: HashSet::new(),
            frozen_cache: FrozenTreeCache::new(),
            root_node_key,
            next_version: current_version,
            reader,
            num_stale_leaves: 0,
            num_new_leaves: 0,
            value_cache: Default::default(),
        })
    }

    /// Gets a node with given node key. If it doesn't exist in node cache, read from `reader`.
    //  TODO(kate): this interface is left as a boxed error, for now.
    pub fn get_node(&self, node_key: &NodeKey) -> Result<Node, anyhow::Error> {
        Ok(if let Some(node) = self.node_cache.get(node_key) {
            node.clone()
        } else if let Some(node) = self.frozen_cache.node_cache.nodes().get(node_key) {
            node.clone()
        } else {
            use crate::storage::TreeReaderExt;
            self.reader.get_node(node_key)?
        })
    }

    /// Gets a node with the given node key. If it doesn't exist in node cache, read from `reader`
    /// If it doesn't exist anywhere, return `None`.
    pub fn get_node_option(&self, node_key: &NodeKey) -> Result<Option<Node>, R::Error> {
        Ok(if let Some(node) = self.node_cache.get(node_key) {
            Some(node.clone())
        } else if let Some(node) = self.frozen_cache.node_cache.nodes().get(node_key) {
            Some(node.clone())
        } else {
            self.reader.get_node_option(node_key)?
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

    /// Puts the node with given hash as key into the inner `node_cache`.
    ///
    /// This function returns an `Err(())` to signal that a node with the provided [`NodeKey`]
    /// already existed in the node cache.
    pub fn put_node(&mut self, node_key: NodeKey, new_node: Node) -> Result<(), NodeAlreadyExists> {
        match self.node_cache.entry(node_key) {
            Entry::Vacant(o) => {
                if new_node.is_leaf() {
                    self.num_new_leaves += 1
                }
                o.insert(new_node);
            }
            Entry::Occupied(o) => {
                return Err(NodeAlreadyExists {
                    key: o.key().to_owned(),
                    node: new_node,
                })
            }
        };

        Ok(())
    }

    pub fn put_value(&mut self, version: Version, key_hash: KeyHash, value: Option<OwnedValue>) {
        self.value_cache.insert((version, key_hash), value);
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
    pub fn freeze<H: SimpleHasher>(&mut self) -> Result<(), FreezeError<R::Error>> {
        let mut root_node_key = self.get_root_node_key().clone();

        let root_node = if let Some(root_node) = self
            .get_node_option(&root_node_key)
            .map_err(FreezeError::GetNodeFailed)?
        {
            root_node
        } else {
            // If the root node does not exist, then we need to set it to the null node and record
            // that node hash as the root hash of this version. This will happen if you delete as
            // the first operation on an empty tree, but also if you manage to delete every single
            // key-value mapping in the tree.
            self.put_node(root_node_key.clone(), Node::new_null())
                .map_err(FreezeError::PutNodeFailed)?;
            Node::Null
        };

        // Insert the root node's hash into the list of root hashes in the frozen cache, so that
        // they can be extracted later after a sequence of transactions:
        self.frozen_cache
            .root_hashes
            .push(RootHash(root_node.hash::<H>()));

        // If the effect of this set of changes has been to do nothing, we still need to create a
        // new root node that matches the anticipated version; we do this by copying the previous
        // root node and incrementing the version. If we didn't do this, then any set of changes
        // which failed to have an effect on the tree would mean that the *next* set of changes
        // would be faced with a non-existent root node at the version it is expecting, since it's
        // internally expected that the version increments every time the tree cache is frozen.
        if self.next_version > 0
            && self.node_cache.is_empty()
            && self.stale_node_index_cache.is_empty()
        {
            let root_node = self
                .get_node_option(&self.root_node_key)
                .map_err(FreezeError::GetNodeFailed)?
                .ok_or(FreezeError::RootNodeMissing)?;
            root_node_key.set_version(self.next_version);
            self.put_node(root_node_key, root_node)
                .map_err(FreezeError::PutNodeFailed)?;
        }

        // Transfer all the state from this version of the cache into the immutable version of the
        // cache, draining it and resetting it as we go:
        let node_stats = NodeStats {
            new_nodes: self.node_cache.len(),
            new_leaves: self.num_new_leaves,
            stale_nodes: self.stale_node_index_cache.len(),
            stale_leaves: self.num_stale_leaves,
        };
        self.frozen_cache.node_stats.push(node_stats);
        self.frozen_cache
            .node_cache
            .extend(self.node_cache.drain(), self.value_cache.drain());
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

        Ok(())
    }
}

impl<'a, R> TreeReader for TreeCache<'a, R>
where
    R: 'a + TreeReader,
    <R as TreeReader>::Error: std::error::Error + Send + Sync + 'static,
{
    type Error = <R as TreeReader>::Error;

    fn get_node_option(&self, node_key: &NodeKey) -> Result<Option<Node>, Self::Error> {
        self.get_node_option(node_key)
    }

    fn get_value_option(
        &self,
        max_version: Version,
        key_hash: KeyHash,
    ) -> Result<Option<OwnedValue>, Self::Error> {
        for ((version, _hash), value) in self
            .value_cache
            .iter()
            .filter(|((_version, hash), _value)| *hash == key_hash)
        {
            if *version <= max_version {
                return Ok(value.clone());
            }
        }

        self.reader.get_value_option(max_version, key_hash)
    }

    fn get_rightmost_leaf(
        &self,
    ) -> Result<Option<(NodeKey, crate::storage::LeafNode)>, Self::Error> {
        unimplemented!("get_rightmost_leaf should not be used with a tree cache")
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
