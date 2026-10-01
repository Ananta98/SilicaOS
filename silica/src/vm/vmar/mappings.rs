// SPDX-License-Identifier: GPL-2.0

//! The mappings of a virtual address space.
//!
//! A [`VmMapping`] is the record of one contiguous run of virtual addresses
//! with a single set of properties. A [`Mappings`] holds those records ordered
//! by start address, guaranteed to be pairwise non-overlapping, and is the only
//! place that maintains that guarantee.
//!
//! # Why one method does almost everything
//!
//! `munmap`, `mprotect`, `mremap` and a `MAP_FIXED` `mmap` all do the same
//! three-step dance: pull out the records that intersect a range, split each of
//! them so the range is covered exactly, and put back whatever should survive.
//! [`Mappings::carve`] is that dance, once. Each caller becomes a single
//! closure saying what happens to the part that is being changed, which is why
//! the three operations are a handful of lines each and stay in step.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{num::NonZeroUsize, ops::Range};

use ostd::mm::{CachePolicy, PAGE_SIZE, PageFlags, PageProperty, Vaddr, VmSpace, tlb::TlbFlushOp};
use ostd::task::disable_preempt;

use crate::vm::{backing::Backing, perms::VmPerms};

use super::intersect;

/// One contiguous run of virtual addresses sharing a set of properties.
///
/// A `VmMapping` is a linear value: it is never dropped while it still owns
/// page table entries, because dropping it would silently leak them. Callers
/// therefore hand ownership on deliberately — [`Self::unmap`] releases the
/// pages, and [`Mappings`] releases the record when a piece is discarded.
pub struct VmMapping {
    /// The first virtual address of the mapping.
    start: Vaddr,
    /// The length in bytes. Always non-zero, so the record can never represent
    /// an empty range.
    len: NonZeroUsize,
    /// The permissions in force, together with the ceiling `mprotect` may not
    /// exceed.
    perms: VmPerms,
    /// Whether writes are carried through to the backing object and are visible
    /// to every other mapping of it.
    shared: bool,
    /// The object supplying the pages, or `None` for `MAP_PRIVATE` anonymous
    /// memory, where every fault gets a fresh zeroed page.
    backing: Option<Arc<dyn Backing>>,
    /// The offset within the backing object that `start` maps to.
    ///
    /// A mapping may be larger than its backing, in which case the tail has no
    /// backing and faults give private zeroed pages.
    backing_offset: usize,
}

impl VmMapping {
    /// Creates a record. `len` must be non-zero and a multiple of the page size.
    pub(super) fn new(
        start: Vaddr,
        len: NonZeroUsize,
        perms: VmPerms,
        shared: bool,
        backing: Option<Arc<dyn Backing>>,
        backing_offset: usize,
    ) -> Self {
        debug_assert!(start.is_multiple_of(PAGE_SIZE));
        debug_assert!(len.get().is_multiple_of(PAGE_SIZE));
        debug_assert!(backing_offset.is_multiple_of(PAGE_SIZE));
        Self {
            start,
            len,
            perms,
            shared,
            backing,
            backing_offset,
        }
    }

    /// Returns the virtual address range this record covers.
    pub fn range(&self) -> Range<Vaddr> {
        self.start..self.end()
    }

    /// Returns the first address past the end of this mapping.
    pub fn end(&self) -> Vaddr {
        self.start + self.len.get()
    }

    /// Returns the length in bytes.
    ///
    /// A mapping is never empty, so there is deliberately no `is_empty`.
    #[expect(clippy::len_without_is_empty, reason = "a mapping is never empty")]
    pub fn len(&self) -> usize {
        self.len.get()
    }

    /// Returns the start address.
    pub fn start(&self) -> Vaddr {
        self.start
    }

    /// Returns the permissions of this mapping.
    pub fn perms(&self) -> VmPerms {
        self.perms
    }

    /// Replaces the permissions in force, keeping the ceiling in place.
    pub(super) fn set_perms(&mut self, perms: VmPerms) {
        self.perms = perms;
    }

    /// Returns whether this mapping is shared with other mappings of its
    /// backing object.
    pub fn is_shared(&self) -> bool {
        self.shared
    }

    /// Returns the object supplying the pages, if any.
    pub fn backing(&self) -> Option<&Arc<dyn Backing>> {
        self.backing.as_ref()
    }

    /// Returns the offset within the backing object that `start` maps to.
    pub fn backing_offset(&self) -> usize {
        self.backing_offset
    }

    /// Returns whether the offset lies within the backing object.
    ///
    /// Addresses past the end of the object are mapped, but have no backing, so
    /// a fault there produces a fresh private page instead of touching the
    /// object.
    pub fn is_backed(&self, offset_in_mapping: usize) -> bool {
        match &self.backing {
            Some(backing) => self.backing_offset + offset_in_mapping < backing.size(),
            None => false,
        }
    }

    /// Returns the offset in the backing object that the address
    /// `start + offset_in_mapping` maps to.
    pub(super) fn backing_offset_of(&self, offset_in_mapping: usize) -> usize {
        self.backing_offset + offset_in_mapping
    }

    /// Returns whether a write to this mapping must copy the page first.
    ///
    /// A shared mapping is never copied: the whole point is that the write
    /// reaches the other mappings.
    pub fn is_cow(&self) -> bool {
        !self.shared
    }

    /// Returns the page table flags that this mapping's permissions imply.
    pub fn page_flags(&self) -> PageFlags {
        PageFlags::from(self.perms)
    }

    /// Grows the mapping by `extra` bytes at its high end.
    pub(super) fn enlarge(self, extra: usize) -> Self {
        debug_assert!(extra.is_multiple_of(PAGE_SIZE));
        Self {
            len: NonZeroUsize::new(self.len.get() + extra).expect("a mapping is never empty"),
            ..self
        }
    }

    /// Moves the record to `new_start`, keeping its properties and backing.
    ///
    /// The page table entries have to be moved separately; see
    /// `Vmar::move_pt`.
    pub(super) fn relocate(self, new_start: Vaddr) -> Self {
        debug_assert!(new_start.is_multiple_of(PAGE_SIZE));
        Self {
            start: new_start,
            ..self
        }
    }

    /// Shifts the backing offset by `delta` bytes.
    pub(super) fn rebase(&mut self, delta: usize) {
        debug_assert!(delta.is_multiple_of(PAGE_SIZE));
        self.backing_offset += delta;
    }

    /// Returns an independent record with the same properties.
    pub(crate) fn dup(&self) -> Self {
        Self {
            start: self.start,
            len: self.len,
            perms: self.perms,
            shared: self.shared,
            backing: self.backing.clone(),
            backing_offset: self.backing_offset,
        }
    }

    /// Splits the record at `at`, which must be strictly inside it.
    pub(super) fn split(self, at: Vaddr) -> (Self, Self) {
        debug_assert!(self.start < at && at < self.end());
        debug_assert!(at.is_multiple_of(PAGE_SIZE));

        let Self {
            start,
            len,
            perms,
            shared,
            backing,
            backing_offset,
        } = self;
        let left_len = at - start;

        let left = Self {
            start,
            len: NonZeroUsize::new(left_len).expect("split leaves a non-empty side"),
            perms,
            shared,
            backing: backing.clone(),
            backing_offset,
        };
        let right = Self {
            start: at,
            len: NonZeroUsize::new(len.get() - left_len).expect("split leaves a non-empty side"),
            perms,
            shared,
            backing,
            backing_offset: backing_offset + left_len,
        };
        (left, right)
    }

    /// Splits the record so that `sub` is covered by exactly the middle result.
    ///
    /// Returns `(outside-left, within, outside-right)`, either side being
    /// `None` when the record ends there.
    pub(super) fn split_range(self, sub: &Range<Vaddr>) -> (Option<Self>, Self, Option<Self>) {
        let mine = self.range();
        assert!(
            mine.start <= sub.start && sub.end <= mine.end,
            "the splitting range must lie inside the mapping"
        );

        if sub.start == mine.start && sub.end == mine.end {
            (None, self, None)
        } else if mine.start < sub.start {
            let (left, rest) = self.split(sub.start);
            if sub.end < mine.end {
                let (middle, right) = rest.split(sub.end);
                (Some(left), middle, Some(right))
            } else {
                (Some(left), rest, None)
            }
        } else if sub.end < mine.end {
            let (middle, right) = self.split(sub.end);
            (None, middle, Some(right))
        } else {
            (None, self, None)
        }
    }

    /// Returns whether `left` and `right` are adjacent records that may be
    /// represented by a single one.
    ///
    /// Two records merge when they are contiguous in the address space and
    /// indistinguishable in every other respect, so that the set stays as
    /// canonical as possible and lookups find long runs.
    pub(super) fn can_merge(left: &Self, right: &Self) -> bool {
        left.end() == right.start
            && left.shared == right.shared
            && left.perms == right.perms
            && match (&left.backing, &right.backing) {
                (None, None) => true,
                (Some(left_backing), Some(right_backing)) => {
                    Arc::ptr_eq(left_backing, right_backing)
                        && left.backing_offset + left.len() == right.backing_offset
                }
                _ => false,
            }
    }

    /// Returns a record covering `left` immediately followed by `right`.
    ///
    /// The caller must have checked [`Self::can_merge`].
    pub(super) fn merged(left: &Self, right: &Self) -> Self {
        debug_assert!(Self::can_merge(left, right));
        Self {
            start: left.start,
            len: NonZeroUsize::new(left.len() + right.len()).expect("both sides are non-empty"),
            perms: left.perms,
            shared: left.shared,
            backing: left.backing.clone(),
            backing_offset: left.backing_offset,
        }
    }

    /// Removes this mapping's page table entries and returns how many pages
    /// were released.
    pub(super) fn unmap(&self, vm_space: &VmSpace) -> usize {
        let range = self.range();
        let guard = disable_preempt();
        let mut cursor = vm_space
            .cursor_mut(&guard, &range)
            .expect("the mapping lies inside the VM space");
        let num_unmapped = cursor.unmap(range.len());
        cursor.flusher().sync_tlb_flush();
        num_unmapped
    }

    /// Applies `perms` to the page table entries of this mapping.
    ///
    /// The write bit is always cleared, whatever the new permissions are. A
    /// mapping is only ever widened by the page-fault handler, which is the one
    /// place that can tell whether the page has to be copied first; a private
    /// page shared with a sibling mapping must stay read-only until then.
    pub(super) fn protect(&self, vm_space: &VmSpace, perms: VmPerms) {
        let new_flags = PageFlags::from(perms) - PageFlags::W;
        let range = self.range();
        let guard = disable_preempt();
        let mut cursor = vm_space
            .cursor_mut(&guard, &range)
            .expect("the mapping lies inside the VM space");

        while cursor.virt_addr() < range.end {
            let remaining = range.end - cursor.virt_addr();
            match cursor.protect_next(remaining, |flags, _| *flags = new_flags) {
                Some(protected) => cursor
                    .flusher()
                    .issue_tlb_flush(TlbFlushOp::for_range(protected)),
                None => break,
            }
        }
        cursor.flusher().dispatch_tlb_flush();
        cursor.flusher().sync_tlb_flush();
    }

    /// Returns the page property that a fault on this mapping installs, minus
    /// the write bit if the page must stay read-only.
    pub(super) fn fault_property(&self, writable: bool, is_write: bool) -> PageProperty {
        let mut flags = self.page_flags() | PageFlags::ACCESSED;
        if is_write {
            flags |= PageFlags::DIRTY;
        }
        if !writable {
            flags -= PageFlags::W;
        }
        PageProperty::new_user(flags, CachePolicy::Writeback)
    }
}

impl core::fmt::Debug for VmMapping {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VmMapping")
            .field("range", &self.range())
            .field("perms", &self.perms)
            .field("shared", &self.shared)
            .field("backing_offset", &self.backing_offset)
            .field("backing", &self.backing)
            .finish()
    }
}

/// What [`Mappings::carve`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CarveOutcome {
    /// The number of bytes of the requested range that were mapped.
    pub mapped: usize,
    /// The number of bytes that `op` chose to discard.
    pub dropped: usize,
}

/// The records of a virtual address space, ordered by start address.
///
/// The records are always pairwise non-overlapping, and neighbours that could
/// be represented by one record always are. Every mutation goes through
/// [`Self::insert`], so the two invariants cannot be broken by a caller.
#[derive(Debug, Default)]
pub(super) struct Mappings {
    /// Keyed by the start address, which is also the record's key in practice
    /// because a `BTreeMap` needs the key to be the ordering.
    map: BTreeMap<Vaddr, VmMapping>,
}

impl Mappings {
    /// Returns an empty set.
    pub(super) fn new() -> Self {
        Self {
            map: BTreeMap::new(),
        }
    }

    /// Returns the number of records.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns the record containing `addr`, if any.
    pub fn get(&self, addr: Vaddr) -> Option<&VmMapping> {
        // Records are non-overlapping, so the only candidate for containing
        // `addr` is the last one that starts at or before it.
        let mapping = self.map.range(..=addr).next_back().map(|(_, m)| m)?;
        (mapping.end() > addr).then_some(mapping)
    }

    /// Returns the last record that starts strictly before `addr`.
    pub fn prev(&self, addr: Vaddr) -> Option<&VmMapping> {
        self.map.range(..addr).next_back().map(|(_, m)| m)
    }

    /// Returns the first record that starts at or after `addr`.
    pub fn next(&self, addr: Vaddr) -> Option<&VmMapping> {
        self.map.range(addr..).next().map(|(_, m)| m)
    }

    /// Returns every record, in ascending address order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &VmMapping> {
        self.map.values()
    }

    /// Returns every record intersecting `range`, in ascending address order.
    pub fn iter_in(&self, range: &Range<Vaddr>) -> Vec<&VmMapping> {
        // Walk down from the first record that could reach `range.start`. Only
        // that one can begin before the range, and because the records do not
        // overlap, the first record that ends too early means every earlier
        // one does too.
        let mut found: Vec<&VmMapping> = self
            .map
            .range(..range.end)
            .rev()
            .take_while(|(_, m)| m.end() > range.start)
            .map(|(_, m)| m)
            .collect();
        found.reverse();
        found
    }

    /// Returns the total length of `range` that is covered by a record.
    pub fn count_overlap(&self, range: &Range<Vaddr>) -> usize {
        self.iter_in(range)
            .iter()
            .map(|m| intersect(range, &m.range()).len())
            .sum()
    }

    /// Returns whether every page of `range` is mapped.
    pub fn is_fully_mapped(&self, range: &Range<Vaddr>) -> bool {
        self.count_overlap(range) == range.len()
    }

    /// Removes and returns every record intersecting `range`.
    pub(super) fn take_in(&mut self, range: &Range<Vaddr>) -> Vec<VmMapping> {
        let keys: Vec<Vaddr> = self.iter_in(range).iter().map(|m| m.start).collect();
        keys.into_iter()
            .map(|key| {
                self.map
                    .remove(&key)
                    .expect("a key came from this very set")
            })
            .collect()
    }

    /// Adds a record, merging it with its neighbours where that is possible.
    ///
    /// Merging only ever joins two contiguous records, so it cannot introduce
    /// an overlap; the debug assertion catches a caller that hands us a record
    /// straddling an existing one.
    pub(super) fn insert(&mut self, mapping: VmMapping) {
        let mut mapping = mapping;

        let prev = self
            .prev(mapping.start)
            .filter(|prev| VmMapping::can_merge(prev, &mapping))
            .map(|prev| prev.start);
        if let Some(key) = prev {
            let left = self
                .map
                .remove(&key)
                .expect("a key came from this very set");
            mapping = VmMapping::merged(&left, &mapping);
        }

        let next = self
            .next(mapping.end())
            .filter(|next| VmMapping::can_merge(&mapping, next))
            .map(|next| next.start);
        if let Some(key) = next {
            let right = self
                .map
                .remove(&key)
                .expect("a key came from this very set");
            mapping = VmMapping::merged(&mapping, &right);
        }

        let replaced = self.map.insert(mapping.start, mapping);
        debug_assert!(replaced.is_none(), "an overlapping record was inserted");
    }

    /// Removes the record starting at `start`.
    pub(super) fn remove(&mut self, start: Vaddr) -> Option<VmMapping> {
        self.map.remove(&start)
    }

    /// Removes every record.
    pub(super) fn clear(&mut self) {
        self.map.clear();
    }

    /// Applies `op` to the part of every record that intersects `range`.
    ///
    /// Each intersected piece is removed from the set, split so that `range` is
    /// covered exactly, and handed to `op` with the sub-range it covers.
    /// Whatever `op` returns is put back and merged with its neighbours;
    /// returning `None` discards the piece, which is how `munmap` releases the
    /// pages. The pieces outside `range` are always preserved.
    ///
    /// Returns how much of `range` was mapped and how much `op` discarded.
    pub(super) fn carve<F>(&mut self, range: &Range<Vaddr>, mut op: F) -> CarveOutcome
    where
        F: FnMut(VmMapping, &Range<Vaddr>) -> Option<VmMapping>,
    {
        assert!(range.start.is_multiple_of(PAGE_SIZE) && range.end.is_multiple_of(PAGE_SIZE));
        assert!(range.start < range.end);

        let mut outcome = CarveOutcome::default();
        let mut survivors = Vec::new();

        for piece in self.take_in(range) {
            let sub = intersect(range, &piece.range());
            outcome.mapped += sub.len();

            let (left, middle, right) = piece.split_range(&sub);
            survivors.extend(left);
            survivors.extend(right);
            match op(middle, &sub) {
                Some(kept) => survivors.push(kept),
                None => outcome.dropped += sub.len(),
            }
        }

        for survivor in survivors {
            self.insert(survivor);
        }
        outcome
    }
}
