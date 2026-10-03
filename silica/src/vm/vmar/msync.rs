// SPDX-License-Identifier: GPL-2.0

//! `msync(2)`.
//!
//! `msync` asks the kernel to bring a shared mapping in line with the object
//! behind it. It has two jobs, and the flag says which is meant:
//!
//! - `MS_SYNC` and `MS_ASYNC` write back whatever the mapping has changed. That
//!   is [`Backing::flush`], which does nothing for an object that is memory.
//! - `MS_INVALIDATE` drops the mapping's page table entries, so a later access
//!   re-reads the object instead of trusting what is there.
//!
//! # What this is worth for a memory-backed object
//!
//! Less than the name suggests, and it is better to say so than to leave a caller
//! guessing. Every mapping of one object points at the same physical pages, so
//! there is no second copy to bring up to date and nothing cached to discard.
//! `MS_SYNC` over a [`SharedPages`](crate::vm::shm::SharedPages) therefore does
//! nothing at all, and `MS_INVALIDATE` amounts to `MADV_DONTNEED` on the page
//! table: the entries go, and the next fault installs the same frames again.
//!
//! What it *is* worth is the argument validation, the `flush` seam a file-backed
//! object will fill in, and the page table invalidation a file backing needs in
//! order to honour `MS_INVALIDATE` properly. Once such a backing exists the code
//! below already does the right thing.
//!
//! # `MS_ASYNC`
//!
//! `MS_ASYNC` promises only that the writeback has been *started*. There is no
//! I/O layer to start it with, so this performs the work and returns once it is
//! done — that is, `MS_ASYNC` behaves as `MS_SYNC`. The deviation is forced by
//! what the kernel has rather than chosen: a caller relying on `MS_ASYNC` to
//! return early would instead wait, which is slower but never wrong.

use core::ops::Range;

use ostd::mm::{PAGE_SIZE, Vaddr};

use crate::{api::errno::Result, vm::flags::MsyncFlags};

use super::{Vmar, is_mappable_range, mappings::VmMapping};

impl Vmar {
    /// Brings the mapping of `range` in line with the object behind it.
    ///
    /// The end of `range` need not be page-aligned; like Linux, the length is
    /// rounded up so a partial trailing page is covered. A length of zero
    /// succeeds and does nothing, which is what `msync(addr, 0, ...)` does.
    pub fn msync(&self, flags: MsyncFlags, addr: Vaddr, len: usize) -> Result<()> {
        // Checked here as well as in `from_raw`, because a caller that assembles
        // the flags itself must not be able to slip past the rule.
        flags.validate()?;

        if !addr.is_multiple_of(PAGE_SIZE) {
            crate::return_errno!(EINVAL, "the address {addr:#x} is not page-aligned");
        }
        let Some(len) = len.checked_next_multiple_of(PAGE_SIZE) else {
            crate::return_errno!(EINVAL, "a length of {len} bytes from {addr:#x} overflows");
        };
        if len == 0 {
            return Ok(());
        }
        let Some(end) = addr.checked_add(len) else {
            crate::return_errno!(EINVAL, "a length of {len} bytes from {addr:#x} overflows");
        };
        let range = addr..end;

        if !is_mappable_range(&range) {
            crate::return_errno!(
                ENOMEM,
                "the range {range:#x?} is not inside the user address space"
            );
        }
        {
            let inner = self.inner.read();
            if !inner.mappings.is_fully_mapped(&range) {
                let missing = range.len() - inner.mappings.count_overlap(&range);
                crate::return_errno!(
                    ENOMEM,
                    "{missing:#x} of the {}-byte range are not mapped",
                    range.len()
                );
            }
        }

        // Copies, so that the address space lock is not held across the flush:
        // that is the one step that may block, and it is the one that does not
        // need the lock.
        let mappings: alloc::vec::Vec<VmMapping> = self
            .inner
            .read()
            .mappings
            .iter_in(&range)
            .iter()
            .map(|mapping| mapping.dup())
            .collect();

        // Write back first, so that an invalidation cannot discard a dirty page
        // that was never pushed out.
        for mapping in &mappings {
            if let Some(backing) = mapping.backing()
                && let Some(object) = object_range(mapping, &range, backing.size())
            {
                backing.flush(object)?;
            }
        }

        if flags.contains(MsyncFlags::INVALIDATE) {
            let mut inner = self.inner.write();
            for mapping in &mappings {
                let part = super::intersect(&range, &mapping.range());
                inner.unmap_pages_only(&self.vm_space, &part);
            }
        }
        Ok(())
    }
}

/// Returns the part of `wanted` that `mapping` backs, in object offsets, or
/// `None` if the two do not overlap the object at all.
///
/// A mapping may be larger than its object, and the pages past the object's end
/// are private, so they take no part in a flush.
fn object_range(
    mapping: &VmMapping,
    wanted: &Range<Vaddr>,
    object_size: usize,
) -> Option<Range<usize>> {
    let part = super::intersect(wanted, &mapping.range());
    // Translate the virtual sub-range into object offsets: the mapping's own
    // offset applies at its start, so the sub-range's start is that much further
    // in.
    let start = mapping.backing_offset() + (part.start - mapping.start());
    let backed = start..start + part.len().min(object_size.saturating_sub(start));
    (!backed.is_empty()).then_some(backed)
}
