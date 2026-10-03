// SPDX-License-Identifier: GPL-2.0

//! The objects that supply physical pages to mappings.
//!
//! A mapping either has no backing object at all, in which case it is
//! `MAP_PRIVATE` anonymous memory and every fault allocates a fresh zeroed
//! page, or it refers to a [`Backing`], in which case the frames the mapping
//! installs come from that object and are shared with every other mapping of
//! it.
//!
//! Only `size` and `frame` have to be implemented. Ranges are populated by
//! driving the ordinary page-fault path (see `Vmar::populate_range`), so a
//! backing never needs to know how to batch or pre-fault; and `flush` has nothing
//! to do for an object that is already memory, so it defaults to doing nothing.
//! A file-backed implementation overrides `flush` to write out, and can add a
//! readahead method without disturbing either.

use core::{fmt::Debug, ops::Range};
use ostd::mm::{FrameAllocOptions, UFrame};
use crate::api::errno::Result;

/// A source of physical pages for a mapping.
///
/// Implementations must be safe to call concurrently from page-fault handlers
/// on different CPUs, and `frame` must return the *same* page for the same
/// offset every time so that two mappings of one object alias.
pub trait Backing: Send + Sync + Debug {
    /// Returns the size of the object in bytes.
    ///
    /// A mapping may be larger than its backing, in which case the pages past
    /// the end are not backed by the object.
    fn size(&self) -> usize;

    /// Returns the page that backs `offset`, materializing it if needed.
    ///
    /// `offset` is always a multiple of the page size and less than
    /// [`Self::size`].
    fn frame(&self, offset: usize) -> Result<UFrame>;

    /// Makes every change to `range` visible to a later read of the object,
    /// whether through this mapping or any other.
    ///
    /// This is what `msync(2)` is for, and it is where a file-backed object
    /// writes its pages out. An object with nowhere to push to, such as plain
    /// memory, does nothing: its pages are already the one true copy, so there
    /// is no separate copy to bring up to date.
    ///
    /// `range` is page-aligned and lies within [`Self::size`]. An implementation
    /// must be safe to call from a task that is not holding any lock on this
    /// object.
    fn flush(&self, _range: Range<usize>) -> Result<()> {
        Ok(())
    }
}

/// Allocates a single zero-filled page from the physical memory allocator.
pub fn alloc_zeroed_frame() -> Result<UFrame> {
    Ok(FrameAllocOptions::new().alloc_frame()?.into())
}
