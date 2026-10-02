// SPDX-License-Identifier: GPL-2.0

//! Giving memory back.
//!
//! The fault path allocates a frame per fault, and until this module existed
//! there was nothing anywhere that took one back: physical memory was only ever
//! spent, so the first allocation to find the allocator empty ended in `ENOMEM`
//! with no attempt to recover.
//!
//! # What can be given back
//!
//! A page may only be dropped if a later fault produces the same bytes, because
//! nothing else holds a copy. That is a per-object question, so it is answered
//! by each [`ReclaimSource`]; the only one in the tree today is
//! [`Vmar`](crate::vm::vmar::Vmar), and it releases clean pages of private
//! anonymous memory. What is excluded, and why, is documented where it is
//! decided, in `vmar::reclaim`.
//!
//! # Why this is a registry and not a call
//!
//! Reclaim has to find every address space in the kernel without being handed
//! them, because the caller that discovers it is out of memory has no idea which
//! one holds something reclaimable. Sources register themselves and are held
//! weakly, so a source that goes away simply drops out of the next pass without
//! needing a matching deregistration.
//!
//! # Why the fault path does not call this
//!
//! Resolving a fault holds the address space's read lock across the frame
//! allocation, and reclaiming that same address space needs its write lock, so a
//! reclaim pass run from inside a fault would deadlock against itself.
//!
//! This is the same reason Linux does not reclaim inline in the fault path: it
//! wakes `kswapd` instead. The equivalent seam here is
//! [`reclaim_at_least`], which a reclaim thread — or any caller that holds no
//! address space lock — can drive. What *is* wired up in-tree is
//! [`Vmar::populate_range`](crate::vm::vmar::Vmar::populate_range), which backs
//! `MAP_POPULATE` and `MADV_WILLNEED` and already faults pages with no lock
//! held: it retries a failed page once after a reclaim pass.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use ostd::sync::SpinLock;
use ostd::sync::WaitQueue;

/// Something the kernel can ask for memory back.
///
/// `budget` is an upper bound on how many pages this source should release, so
/// that one source cannot satisfy a whole system's need while another holds pages
/// the caller would rather have. The return value is the number of pages actually
/// released: it may be zero, it may be less than `budget`, and a source that
/// returns more than it was asked for has its excess disregarded.
///
/// Implementations must not block indefinitely, and must not release a page they
/// have not established is safe to drop. The page table entry is what makes a
/// page reachable, so a wrongly chosen one loses data rather than merely losing
/// performance.
pub trait ReclaimSource: Send + Sync {
    /// Releases up to `budget` pages, returning how many were released.
    fn reclaim(&self, budget: usize) -> usize;
}

/// The sources a pass asks, keyed by a slot so that nothing has to be found again
/// by value.
///
/// The values are weak, which is why this can live in a static: a dropped source
/// is forgotten rather than kept alive.
type Registry = SpinLock<BTreeMap<usize, Weak<dyn ReclaimSource>>>;

static SOURCES: Registry = SpinLock::new(BTreeMap::new());

/// Wait queue for kswapd daemon.
pub static KSWAPD_WAIT_QUEUE: WaitQueue = WaitQueue::new();

/// Flag to signal kswapd to wake up.
pub static KSWAPD_WAKEUP_FLAG: AtomicBool = AtomicBool::new(false);

/// The next free slot in [`SOURCES`].
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);

/// The total number of pages released since boot.
static PAGES_RECLAIMED: AtomicUsize = AtomicUsize::new(0);

/// The number of passes run since boot.
static PASSES: AtomicUsize = AtomicUsize::new(0);

/// Registers `source` as something [`reclaim`] asks.
///
/// The registry holds only a weak reference, so registering does not keep an
/// address space alive. Nothing has to be unregistered: a source that has been
/// dropped is skipped, and its slot pruned, by the next pass.
///
/// Registering the same source twice is harmless — the second call takes a fresh
/// slot and a pass simply asks the source twice, wasting part of one pass. That
/// is cheaper than a de-duplication table which would itself need pruning.
pub fn register<S: ReclaimSource + 'static>(source: &Arc<S>) {
    let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
    SOURCES
        .lock()
        .insert(slot, Arc::downgrade(source) as Weak<dyn ReclaimSource>);
}

/// Returns the total number of pages released by reclaim since boot.
pub fn pages_reclaimed() -> usize {
    PAGES_RECLAIMED.load(Ordering::Relaxed)
}

/// Returns the number of reclaim passes run since boot.
pub fn passes() -> usize {
    PASSES.load(Ordering::Relaxed)
}

/// Runs one reclaim pass and returns how many pages it released.
///
/// Every registered source is asked in turn for whatever is left of `budget`, so
/// one source cannot take the whole allowance and leave a later one nothing. A
/// pass that releases nothing means there is nothing left to release, which is
/// what [`reclaim_at_least`] stops on.
pub fn reclaim(budget: usize) -> usize {
    PASSES.fetch_add(1, Ordering::Relaxed);
    if budget == 0 {
        return 0;
    }

    // Forgetting the sources that have died keeps this map from growing without
    // bound as address spaces come and go.
    let mut sources = SOURCES.lock();
    sources.retain(|_, source| source.strong_count() > 0);

    let mut remaining = budget;
    let mut freed = 0;
    for source in sources.values() {
        if remaining == 0 {
            break;
        }
        let Some(source) = source.upgrade() else {
            continue;
        };
        // A source that over-reports would make the accounting a lie and the
        // budget meaningless, so the total is clamped rather than trusted.
        let released = source.reclaim(remaining).min(remaining);
        freed += released;
        remaining -= released;
    }

    PAGES_RECLAIMED.fetch_add(freed, Ordering::Relaxed);
    freed
}

/// Runs reclaim passes until `pages` have been released or a pass comes back
/// empty-handed, returning how many pages were released in total.
///
/// This cannot wait for progress that will not arrive: the loop ends as soon as
/// a pass releases nothing, so a caller that cannot be satisfied still returns.
pub fn reclaim_at_least(pages: usize) -> usize {
    let mut freed = 0;
    while freed < pages {
        let released = reclaim(pages - freed);
        if released == 0 {
            break;
        }
        freed += released;
    }
    freed
}

/// Spawns the background page daemon (`kswapd0`).
pub fn init_kswapd() {
    crate::proc::kthread::kproc_create(
        "kswapd0",
        || loop {
            let _ = reclaim_at_least(32);

            // Wait for explicit wakeup request from the memory allocator
            KSWAPD_WAIT_QUEUE.wait_until(|| {
                if KSWAPD_WAKEUP_FLAG.swap(false, Ordering::Acquire) {
                    Some(())
                } else {
                    None
                }
            });
        },
        19, // Lowest priority (nice 19)
    )
    .expect("failed to spawn kswapd0");
}

/// Wakes up the background page daemon (`kswapd0`) to reclaim memory.
/// This should be called by the page allocator when free memory is low.
pub fn wakeup_kswapd() {
    KSWAPD_WAKEUP_FLAG.store(true, Ordering::Release);
    KSWAPD_WAIT_QUEUE.wake_one();
}
