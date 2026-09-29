// SPDX-License-Identifier: GPL-2.0

//! Virtual memory address regions.
//!
//! A [`Vmar`] is the address space of one address space, that is, of one
//! process or one thread that does not share one. It owns an
//! [`ostd::mm::VmSpace`] — the hardware page table — and the [`Mappings`] that
//! describe what has been placed in it.
//!
//! Every operation here works on page-aligned ranges and reports failures as
//! POSIX [`Errno`](crate::errno::Errno) values, so that the syscall layer is a
//! thin argument-validation shell around these methods.

mod fork;
mod mmap;
mod mappings;
mod mprotect;
mod mremap;
mod msync;
mod munmap;
pub mod page_fault;

use alloc::sync::Arc;
use core::{
    ops::Range,
    sync::atomic::{AtomicUsize, Ordering},
};

use ostd::mm::{MAX_USERSPACE_VADDR, PAGE_SIZE, Vaddr, VmSpace};
use ostd::sync::{PreemptDisabled, RwLock, RwLockReadGuard};

use crate::errno::{Errno, Result};

pub use self::mappings::VmMapping;
use self::mappings::{CarveOutcome, Mappings};

/// The lowest address a mapping may occupy.
///
/// This mirrors Linux's `mmap_min_addr` default of 64 KiB. Leaving the first
/// page unmapped means that a null-pointer dereference in user space is
/// guaranteed to fault instead of aliasing something.
pub const VMAR_LOWEST_ADDR: Vaddr = 0x0010_0000;

/// The exclusive upper bound on mapping addresses.
pub const VMAR_CAP_ADDR: Vaddr = MAX_USERSPACE_VADDR;

/// The headroom left above all mappings for a process's main stack to grow
/// into, including its guard page and random offset.
const VMAR_STACK_RESERVE: usize = 2048 * PAGE_SIZE;

/// The exclusive upper bound used when `MAP_32BIT` is requested, which asks for
/// an address below 2 GiB.
#[cfg(target_arch = "x86_64")]
const MAP_32BIT_HIGH_LIMIT: Vaddr = 0x8000_0000;

/// Returns whether `range1` and `range2` overlap.
fn is_intersected(range1: &Range<Vaddr>, range2: &Range<Vaddr>) -> bool {
    range1.start.max(range2.start) < range1.end.min(range2.end)
}

/// Returns the overlap of two ranges, which the caller must know is non-empty.
fn intersect(range1: &Range<Vaddr>, range2: &Range<Vaddr>) -> Range<Vaddr> {
    debug_assert!(is_intersected(range1, range2));
    range1.start.max(range2.start)..range1.end.min(range2.end)
}

/// Returns whether `vaddr` can be mapped at all.
fn is_mappable_range(range: &Range<Vaddr>) -> bool {
    range.start >= VMAR_LOWEST_ADDR && range.end <= VMAR_CAP_ADDR
}

/// Checks that `range` is a non-empty, page-aligned range inside user space.
pub(crate) fn check_page_aligned_range(range: &Range<Vaddr>) -> Result<()> {
    if range.start >= range.end {
        crate::return_errno!(EINVAL, "the range {:#x}..{:#x} is empty", range.start, range.end);
    }
    if !range.start.is_multiple_of(PAGE_SIZE) || !range.end.is_multiple_of(PAGE_SIZE) {
        crate::return_errno!(
            EINVAL,
            "the range {:#x}..{:#x} is not page-aligned",
            range.start,
            range.end
        );
    }
    Ok(())
}

/// The address space of a process or thread.
pub struct Vmar {
    /// The records and the total mapped size, which `mmap` checks against the
    /// address space limit while it holds the write lock.
    inner: RwLock<VmarInner>,
    /// The hardware page table.
    vm_space: Arc<VmSpace>,
    /// The address space limit in bytes, or `0` for no limit.
    ///
    /// This is the `RLIMIT_AS` that `mmap` and `mremap` must fail with
    /// `ENOMEM` against. It lives here rather than in the process object so
    /// that the limit is enforced even before the process layer exists.
    max_addr_space: AtomicUsize,
}

/// The state guarded by [`Vmar::inner`].
#[derive(Default)]
struct VmarInner {
    mappings: Mappings,
    /// The total number of mapped bytes, used for the address space limit.
    total_vm: usize,
}

impl Vmar {
    /// Creates a new, empty address space.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(VmarInner {
                mappings: Mappings::new(),
                total_vm: 0,
            }),
            vm_space: Arc::new(VmSpace::new()),
            max_addr_space: AtomicUsize::new(0),
        })
    }

    /// Returns the hardware page table backing this address space.
    pub fn vm_space(&self) -> &Arc<VmSpace> {
        &self.vm_space
    }

    /// Installs this address space on the current CPU.
    ///
    /// OSTD only keeps the activated `VmSpace` current until something else
    /// activates another one, which includes a context switch. The scheduler
    /// therefore has to call this for the incoming task; see
    /// [`crate::vm::init`], which does so automatically.
    pub fn activate(&self) {
        self.vm_space.activate();
    }

    /// Returns the total number of mapped bytes.
    pub fn total_mapped_size(&self) -> usize {
        self.inner.read().total_vm
    }

    /// Returns the address space limit in bytes, or `None` if there is none.
    pub fn max_addr_space(&self) -> Option<usize> {
        match self.max_addr_space.load(Ordering::Relaxed) {
            0 => None,
            limit => Some(limit),
        }
    }

    /// Sets the address space limit in bytes. `None` removes the limit.
    pub fn set_max_addr_space(&self, limit: Option<usize>) {
        self.max_addr_space
            .store(limit.unwrap_or(0), Ordering::Relaxed);
    }

    /// Returns the mappings that intersect `range`, in ascending address order.
    pub fn mappings_in(&self, range: Range<Vaddr>) -> VmarQuery<'_> {
        VmarQuery {
            inner: self.inner.read(),
            range,
        }
    }

    /// Returns whether every page of `range` is mapped.
    pub fn is_fully_mapped(&self, range: &Range<Vaddr>) -> bool {
        self.inner.read().mappings.is_fully_mapped(range)
    }

    /// Releases every mapping and every page table entry.
    ///
    /// This runs when the last reference to the address space goes away, so that
    /// a dropped `Vmar` frees its memory instead of leaking it.
    pub fn clear(&self) {
        let mut inner = self.inner.write();
        let vm_space = &self.vm_space;

        // One cursor over the whole space releases every frame in a single
        // operation, which is far cheaper than walking the records.
        let full_range = 0..VMAR_CAP_ADDR;
        let guard = ostd::task::disable_preempt();
        let mut cursor = vm_space
            .cursor_mut(&guard, &full_range)
            .expect("the VM space covers the whole range");
        cursor.unmap(full_range.len());
        cursor.flusher().sync_tlb_flush();

        inner.mappings.clear();
        inner.total_vm = 0;
    }
}

impl Drop for Vmar {
    fn drop(&mut self) {
        self.clear();
    }
}

/// A borrowed view of the mappings that intersect a range.
///
/// Holding one keeps the records alive and unchanging; drop it as soon as
/// possible, because it blocks every mutating operation on the address space.
pub struct VmarQuery<'a> {
    inner: RwLockReadGuard<'a, VmarInner, PreemptDisabled>,
    range: Range<Vaddr>,
}

impl VmarQuery<'_> {
    /// Returns the intersecting mappings, in ascending address order.
    pub fn iter(&self) -> alloc::vec::IntoIter<&VmMapping> {
        self.inner.mappings.iter_in(&self.range).into_iter()
    }

    /// Returns whether every page of the range is mapped.
    pub fn is_fully_mapped(&self) -> bool {
        self.inner.mappings.is_fully_mapped(&self.range)
    }

    /// Returns how many mappings intersect the range.
    pub fn len(&self) -> usize {
        self.inner.mappings.len()
    }

    /// Returns whether no mapping intersects the range.
    pub fn is_empty(&self) -> bool {
        self.inner.mappings.len() == 0
    }
}

impl VmarInner {
    /// Fails with `ENOMEM` if mapping `extra` more bytes would exceed the
    /// address space limit.
    fn check_fits_addr_space(&self, vmar: &Vmar, extra: usize) -> Result<()> {
        self.check_total_fits(vmar, self.total_vm.saturating_add(extra))
    }

    /// Fails with `ENOMEM` if the address space would end up holding more than
    /// the limit allows.
    ///
    /// This is what an operation that may *shrink* the address space needs, such
    /// as a `mremap` that moves a mapping and resizes it at the same time: the
    /// interesting quantity is where the total ends up, not how far it moves.
    fn check_total_fits(&self, vmar: &Vmar, total: usize) -> Result<()> {
        let Some(limit) = vmar.max_addr_space() else {
            return Ok(());
        };
        if total > limit {
            crate::return_errno!(
                ENOMEM,
                "{total} bytes of mappings would exceed the address space limit of {limit}"
            );
        }
        Ok(())
    }

    /// Applies `op` to the part of every mapping that intersects `range`, as
    /// described by [`Mappings::carve`], and keeps the mapped-size total right.
    fn carve<F>(&mut self, range: &Range<Vaddr>, op: F) -> CarveOutcome
    where
        F: FnMut(VmMapping, &Range<Vaddr>) -> Option<VmMapping>,
    {
        let outcome = self.mappings.carve(range, op);
        self.total_vm -= outcome.dropped;
        outcome
    }

    /// Removes the pages of every mapping that intersects `range`.
    fn unmap_range(&mut self, vm_space: &VmSpace, range: &Range<Vaddr>) {
        self.carve(range, |piece, _| {
            piece.unmap(vm_space);
            None
        });
    }

    /// Drops the page table entries of every mapping that intersects `range`,
    /// keeping the mappings themselves.
    ///
    /// This is what `MADV_DONTNEED` and `MS_INVALIDATE` both want: the address
    /// range stays mapped, so a later access re-faults a page, while the pages
    /// themselves survive if they belong to a shared object that other mappings
    /// are still using.
    fn unmap_pages_only(&mut self, vm_space: &VmSpace, range: &Range<Vaddr>) -> CarveOutcome {
        self.carve(range, |piece, _| {
            piece.unmap(vm_space);
            Some(piece)
        })
    }

    /// Returns a free region of `size` bytes below `high_limit`, searching from
    /// high addresses to low ones.
    ///
    /// Searching downwards is what keeps the gap below the mappings available
    /// for a stack that grows up, and it means a fresh address space hands out
    /// high addresses while the space beneath them stays free.
    fn find_free_region(&self, size: usize, high_limit: Vaddr) -> Result<Range<Vaddr>, NoRoom> {
        /// Returns a region of `size` bytes inside `hole`, aligned down from its
        /// end so that it sits as high in the hole as `align` allows.
        fn take_from_hole(hole: &Range<Vaddr>, size: usize, align: usize) -> Option<Range<Vaddr>> {
            let start = hole.end.checked_sub(size)?.checked_div(align)?.checked_mul(align)?;
            (start >= hole.start).then_some(start..start + size)
        }

        let low_limit = VMAR_LOWEST_ADDR;
        if high_limit <= low_limit || size > high_limit - low_limit {
            return Err(NoRoom);
        }

        // The gap above the highest mapping, then each gap between neighbours.
        let mut ceiling = high_limit;
        for mapping in self.mappings.iter().rev() {
            if let Some(region) = take_from_hole(&(mapping.end()..ceiling), size, PAGE_SIZE) {
                return Ok(region);
            }
            if mapping.start() <= low_limit {
                return Err(NoRoom);
            }
            ceiling = mapping.start();
        }

        take_from_hole(&(low_limit..ceiling), size, PAGE_SIZE).ok_or(NoRoom)
    }

    /// Returns the high address limit for an ordinary mapping.
    fn default_high_limit(&self) -> Vaddr {
        VMAR_CAP_ADDR - VMAR_STACK_RESERVE
    }
}

/// The address space has no room for a mapping of the requested size.
///
/// `mmap` and `mremap` report this as the only errno the specification allows,
/// so the callers convert it with [`NoRoom::into`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NoRoom;

impl From<NoRoom> for Errno {
    fn from(_: NoRoom) -> Errno {
        Errno::ENOMEM
    }
}

#[cfg(ktest)]
mod tests {
    use core::num::NonZeroUsize;

    use ostd::prelude::ktest;

    use super::*;
    use crate::vm::{backing::Backing, perms::VmPerms};

    /// A range of `pages` pages starting at page `first`.
    fn pages(first: usize, count: usize) -> Range<Vaddr> {
        (first * PAGE_SIZE)..((first + count) * PAGE_SIZE)
    }

    #[ktest]
    fn intersection_helpers() {
        // Page 10 covers 10..11, which page 11's range starts inside.
        assert!(is_intersected(&pages(10, 2), &pages(11, 1)));
        // Pages 10 and 11 are adjacent, not overlapping.
        assert!(!is_intersected(&pages(10, 1), &pages(11, 1)));
        assert_eq!(intersect(&pages(10, 2), &pages(11, 1)), pages(11, 1));
        // The intersection is the other way round too.
        assert_eq!(intersect(&pages(11, 1), &pages(10, 2)), pages(11, 1));
    }

    #[ktest]
    fn mappable_range_respects_the_bounds() {
        // `VMAR_LOWEST_ADDR` is 1 MiB, which is page 256.
        assert!(is_mappable_range(&pages(256, 1)));
        assert!(!is_mappable_range(&pages(255, 1)));
        assert!(!is_mappable_range(&pages(0, 1)));
        // The exclusive upper bound: a range that would end above it is out.
        assert!(is_mappable_range(&(VMAR_CAP_ADDR - PAGE_SIZE..VMAR_CAP_ADDR)));
        assert!(!is_mappable_range(&(VMAR_LOWEST_ADDR..VMAR_CAP_ADDR + PAGE_SIZE)));
    }

    #[ktest]
    fn free_region_prefers_the_top() {
        let vmar = Vmar::new();
        let inner = vmar.inner.read();
        let high = inner.default_high_limit();

        let region = inner.find_free_region(PAGE_SIZE, high).unwrap();
        assert_eq!(region.end, high);
    }

    #[ktest]
    fn free_region_skips_occupied_space() {
        let vmar = Vmar::new();
        let high = vmar.inner.read().default_high_limit();
        let mut inner = vmar.inner.write();

        let at_high = high - PAGE_SIZE;
        let record = VmMapping::new(
            at_high,
            NonZeroUsize::new(PAGE_SIZE).unwrap(),
            VmPerms::READ.with_ceiling(),
            false,
            None::<Arc<dyn Backing>>,
            0,
        );
        inner.mappings.insert(record);
        inner.total_vm += PAGE_SIZE;

        // The region below the new record is used, not the occupied page.
        let region = inner.find_free_region(PAGE_SIZE, high).unwrap();
        assert_eq!(region.end, at_high);
    }

    #[ktest]
    fn free_region_fails_when_full() {
        let vmar = Vmar::new();
        let inner = vmar.inner.read();
        let too_big = inner.default_high_limit() - VMAR_LOWEST_ADDR + PAGE_SIZE;
        assert_eq!(
            inner.find_free_region(too_big, inner.default_high_limit()),
            Err(NoRoom)
        );
    }
}
