// SPDX-License-Identifier: GPL-2.0

//! Directory Cache (dcache) abstractions for the VFS.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt::Debug;
use spin::RwLock;

use super::inode::INode;
use super::mount::Mount;

/// Represents the caching state of an inode for a particular directory entry.
#[derive(Default)]
pub enum EntryState {
    /// Entry is positively cached and contains a link to the inode.
    Present(Arc<INode>),
    /// Entry is negatively cached (we know it doesn't exist).
    NotPresent,
    /// The entry hasn't been looked up yet or was evicted.
    #[default]
    NotCached,
}

/// A Directory Cache Entry (dentry).
///
/// Represents a component of a path in the VFS cache.
pub struct DEntry {
    /// The name of this component.
    pub name: String,

    /// The underlying `INode` this entry points to, along with its cache state.
    pub inode: RwLock<EntryState>,

    /// The parent of this `DEntry`. Weak reference prevents cycles.
    pub parent: Option<Weak<DEntry>>,

    /// A map of all cached children of this entry.
    pub children: RwLock<BTreeMap<String, Arc<DEntry>>>,

    /// A list of filesystems mounted on top of this dentry.
    pub mounts: RwLock<Vec<Arc<Mount>>>,
}

impl DEntry {
    /// Creates a new directory entry.
    pub fn new(name: String, inode: Option<Arc<INode>>, parent: Option<Weak<DEntry>>) -> Self {
        let state = match inode {
            Some(node) => EntryState::Present(node),
            None => EntryState::NotPresent,
        };

        DEntry {
            name,
            inode: RwLock::new(state),
            parent,
            children: RwLock::new(BTreeMap::new()),
            mounts: RwLock::new(Vec::new()),
        }
    }

    /// Retrieves the currently cached INode, if any.
    pub fn get_inode(&self) -> Option<Arc<INode>> {
        let lock = self.inode.read();
        match &*lock {
            EntryState::Present(inode) => Some(Arc::clone(inode)),
            _ => None,
        }
    }

    /// Updates the cached INode for this entry.
    pub fn set_inode(&self, inode: Option<Arc<INode>>) {
        let mut lock = self.inode.write();
        *lock = match inode {
            Some(node) => EntryState::Present(node),
            None => EntryState::NotPresent,
        };
    }

    /// Looks up a child in the cache.
    pub fn lookup_child(&self, name: &str) -> Option<Arc<DEntry>> {
        self.children.read().get(name).cloned()
    }

    /// Adds a child to the cache.
    pub fn add_child(&self, child: Arc<DEntry>) {
        self.children.write().insert(child.name.clone(), child);
    }
}

impl Debug for DEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DEntry")
            .field("name", &self.name)
            .finish()
    }
}
