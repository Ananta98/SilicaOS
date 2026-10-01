// SPDX-License-Identifier: GPL-2.0

//! Shared memory objects.
//!
//! A [`SharedPages`] object owns the physical pages that back it, keyed by
//! offset. Two mappings of the same object that cover the same offset install
//! the same frame, so a write through one is immediately visible through the
//! other, and the object survives for exactly as long as it is reachable —
//! which is what `fork(2)` inheritance and `MAP_SHARED` need.
//!
//! [`SharedPages`] serves two roles:
//!
//! - It is the object behind `MAP_SHARED | MAP_ANONYMOUS`. The kernel creates
//!   one for such a mapping and the child inherits it across `fork(2)`.
//! - It is the object behind a *named* shared memory region, reachable through
//!   [`create`], [`open`] and [`unlink`]. These mirror the object half of
//!   `shm_open(3)`/`shm_unlink(3)`; once the `fs` layer exists it will hold
//!   the same `Arc` in a file handle, which is all that is needed to serve the
//!   real system calls.
//!
//! # Lifetime
//!
//! A named object is kept alive by the registry, so it disappears when it is
//! [`unlink`]ed and the last mapping goes away. This matches POSIX, where a
//! named object outlives the file descriptors that opened it. The flip side is
//! that a name that is never unlinked keeps its memory: that is POSIX's rule,
//! not an oversight.

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::fmt::{self, Debug, Formatter};

use ostd::mm::{UFrame, io::util::HasVmReaderWriter};
use ostd::sync::SpinLock;

use crate::{
    errno::Result,
    vm::backing::{Backing, alloc_zeroed_frame},
};

/// The longest name a shared memory object may have, matching `NAME_MAX`.
const NAME_MAX: usize = 255;

/// A fixed-size region of shared physical memory.
///
/// Pages are materialized on first access and then stay resident for the life
/// of the object. An object is never resized; a mapping that would reach past
/// [`Self::size`] is rejected when it is created.
pub struct SharedPages {
    size: usize,
    /// Materialized pages, keyed by their offset in the object.
    ///
    /// A spinlock suffices: the critical section is one map lookup and at most
    /// one insert, and the page-fault path that reaches it runs with
    /// preemption disabled.
    pages: SpinLock<BTreeMap<usize, UFrame>>,
}

impl SharedPages {
    /// Creates an object `size` bytes long.
    pub fn new(size: usize) -> Result<Arc<Self>> {
        if size == 0 {
            crate::return_errno!(EINVAL, "a shared memory object cannot be empty");
        }
        Ok(Arc::new(Self {
            size,
            pages: SpinLock::new(BTreeMap::new()),
        }))
    }

    /// Returns the size of the object in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Returns the number of pages of the object that have been materialized.
    pub fn resident_pages(&self) -> usize {
        self.pages.lock().len()
    }

    /// Returns the `u64` stored at `offset` in the object.
    ///
    /// This bypasses every mapping, so it is the way a test tells whether a
    /// mapping wrote to the object byte it was supposed to.
    pub fn peek_u64(&self, offset: usize) -> Result<u64> {
        let frame = self.frame_at(offset)?;
        let mut reader = frame.reader();
        Ok(reader.read_val()?)
    }

    /// Returns the page at `offset`, materializing it if this is its first use.
    fn frame_at(&self, offset: usize) -> Result<UFrame> {
        if offset >= self.size {
            crate::return_errno!(EINVAL, "offset {offset:#x} is past the end of the object");
        }

        let mut pages = self.pages.lock();
        if let Some(frame) = pages.get(&offset) {
            return Ok(frame.clone());
        }

        // The page is zero-filled, which is what `shm_open` promises for pages
        // that have never been written.
        let frame = alloc_zeroed_frame()?;
        pages.insert(offset, frame.clone());
        Ok(frame)
    }
}

impl Backing for SharedPages {
    fn size(&self) -> usize {
        self.size
    }

    fn frame(&self, offset: usize) -> Result<UFrame> {
        self.frame_at(offset)
    }
}

impl Debug for SharedPages {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let resident = self.pages.lock().len();
        f.debug_struct("SharedPages")
            .field("size", &self.size)
            .field("resident_pages", &resident)
            .finish()
    }
}

/// Creates the unnamed object that backs `MAP_SHARED | MAP_ANONYMOUS`.
///
/// The name is `None` because there is nothing to look the object up by: it
/// lives exactly as long as the mappings that refer to it, which is how
/// `MAP_SHARED` anonymous memory is inherited across `fork(2)` and disappears
/// when the last holder unmaps it.
pub(super) fn new_anonymous(size: usize) -> Result<Arc<SharedPages>> {
    SharedPages::new(size)
}

/// A named shared memory object.
///
/// The registry holds a strong reference, which is what keeps a named object
/// alive after the process that created it exits.
type Registry = SpinLock<Vec<(String, Arc<SharedPages>)>>;

static REGISTRY: Registry = SpinLock::new(Vec::new());

/// Checks that `name` is usable as a shared memory object name.
///
/// POSIX requires a leading slash and forbids interior ones, so that a name can
/// never escape the shared memory namespace.
fn check_name(name: &str) -> Result<()> {
    if name.len() > NAME_MAX {
        crate::return_errno!(
            ENAMETOOLONG,
            "shared memory name is longer than {NAME_MAX} bytes"
        );
    }
    if name.is_empty() || !name.starts_with('/') {
        crate::return_errno!(EINVAL, "a shared memory name must start with '/'");
    }
    if name[1..].contains('/') {
        crate::return_errno!(
            EINVAL,
            "a shared memory name must not contain a '/' after the first"
        );
    }
    Ok(())
}

/// Creates a new named object of `size` bytes.
///
/// Returns [`crate::errno::Errno::EEXIST`] if the name is already taken, and
/// [`crate::errno::Errno::EINVAL`] if `size` is zero.
pub fn create(name: &str, size: usize) -> Result<Arc<SharedPages>> {
    check_name(name)?;
    let object = SharedPages::new(size)?;

    let mut registry = REGISTRY.lock();
    if registry.iter().any(|(existing, _)| existing == name) {
        crate::return_errno!(EEXIST, "shared memory object {name} already exists");
    }
    registry.push((name.to_string(), object.clone()));
    Ok(object)
}

/// Opens an existing named object.
///
/// Returns [`crate::errno::Errno::ENOENT`] if no such object exists.
pub fn open(name: &str) -> Result<Arc<SharedPages>> {
    check_name(name)?;

    let registry = REGISTRY.lock();
    let object = registry
        .iter()
        .find(|(existing, _)| existing == name)
        .map(|(_, object)| object.clone());
    match object {
        Some(object) => Ok(object),
        None => crate::return_errno!(ENOENT, "no shared memory object named {name}"),
    }
}

/// Removes a name from the shared memory namespace.
///
/// The object itself survives until its last mapping is unmapped.
///
/// Returns [`crate::errno::Errno::ENOENT`] if no such object exists.
pub fn unlink(name: &str) -> Result<()> {
    check_name(name)?;

    let mut registry = REGISTRY.lock();
    match registry.iter().position(|(existing, _)| existing == name) {
        Some(index) => {
            registry.remove(index);
            Ok(())
        }
        None => crate::return_errno!(ENOENT, "no shared memory object named {name}"),
    }
}

/// Returns the names of all registered shared memory objects.
pub fn names() -> Vec<String> {
    REGISTRY
        .lock()
        .iter()
        .map(|(name, _)| name.clone())
        .collect()
}
