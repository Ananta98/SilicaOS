// SPDX-License-Identifier: GPL-2.0

//! The scheduling state of one task.
//!
//! A task that the scheduler runs has an [`Entity`] attached to it, which is
//! where the earliest eligible virtual deadline first (EEVDF) bookkeeping lives.
//! An entity is shared between CPUs — a task may be woken on one CPU and run on
//! another — so every field is an atomic.
//!
//! # The algorithm in one paragraph
//!
//! Each task is charged a *virtual runtime* proportional to the CPU time it
//! receives and inversely proportional to its weight, so a task with twice the
//! weight of another advances its virtual runtime at half the rate and
//! therefore receives twice the CPU. The runqueue's *virtual time* is the
//! weighted mean of the virtual runtimes of its runnable tasks, and the
//! difference between a task's own virtual runtime and that mean is its *lag*:
//! positive lag means the task has received less than its share, negative lag
//! that it has received more. A task whose lag is non-negative is *eligible*,
//! and of the eligible tasks the one with the earliest *virtual deadline*
//! runs next. The deadline is the point in virtual time by which the task
//! wants to have finished its request, which is its virtual runtime plus the
//! virtual time its requested time slice is worth; a task that asks for a
//! short slice therefore gets an early deadline and runs sooner, which is how
//! EEVDF buys latency without giving up fairness.
//!
//! All the arithmetic is in whole nanoseconds of virtual time, which for a
//! task of weight [`NICE_0_LOAD`] is the same as real time.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use super::{NSEC_PER_TICK, stats::TaskStats};

/// The load of a task of nice value 0, and the unit in which virtual runtime
/// is counted.
///
/// A task of weight `w` accumulates `1024 / w` virtual nanoseconds per real
/// nanosecond, so two tasks of equal weight share the CPU equally while a task
/// of weight `2 * NICE_0_LOAD` runs at twice the rate of the other one's
/// virtual clock.
pub const NICE_0_LOAD: u32 = 1024;

/// The time slice a task gets if it does not ask for another one, in
/// nanoseconds.
///
/// A slice is the amount of virtual time a task wants to consume before it
/// becomes ineligible and has to queue again, so it is what bounds the
/// latency with which a task waiting its turn is run.
pub const BASE_SLICE_NS: u64 = 3_000_000;

/// The shortest time slice a task may ask for, in nanoseconds.
///
/// Below this a task spends more time being switched than running.
pub const MIN_SLICE_NS: u64 = 100_000;

/// The longest time slice a task may ask for, in nanoseconds.
pub const MAX_SLICE_NS: u64 = 100_000_000;

/// The weight of each nice value, indexed by `nice + 20`.
///
/// This is the `sched_prio_to_weight` table of Linux, which rounds the
/// theoretical `1024 * 1.1^-nice` to a small integer weight. Nice 0, the
/// index 20, is [`NICE_0_LOAD`].
const PRIO_TO_WEIGHT: [u32; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904,
    3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87,
    70, 56, 45, 36, 29, 23, 18, 15,
];

/// The lowest nice value a task may have, which is the highest priority.
pub const NICE_MIN: i8 = -20;

/// The highest nice value a task may have, which is the lowest priority.
pub const NICE_MAX: i8 = 19;

/// Returns the weight of a task of nice value `nice`.
///
/// A lower nice value is a higher priority and yields a larger weight, and so
/// a larger share of the CPU. `nice` is clamped to [`NICE_MIN`]..=[`NICE_MAX`].
pub fn weight_from_nice(nice: i8) -> u32 {
    let index = (nice.clamp(NICE_MIN, NICE_MAX) - NICE_MIN) as usize;
    PRIO_TO_WEIGHT[index]
}

/// Returns the nice value whose weight is `weight`, as [`weight_from_nice`]
/// would have produced it.
///
/// A weight that is not in the table is rounded down to the nearest weight
/// that is, which can only ever cost the task priority.
pub fn nice_from_weight(weight: u32) -> i8 {
    let index = PRIO_TO_WEIGHT
        .iter()
        .position(|w| *w <= weight)
        .unwrap_or(PRIO_TO_WEIGHT.len() - 1);
    NICE_MIN + index as i8
}

/// Hands out the sequence numbers that break ties between tasks whose virtual
/// deadline and virtual runtime are equal.
static NEXT_SEQ: AtomicU32 = AtomicU32::new(0);

/// The scheduling state of one task.
///
/// Every field is atomic because the task's runqueue may be on another CPU:
/// an enqueue on a remote CPU reads the entity to place it, while the owning
/// CPU writes it. The runqueue lock orders those accesses for everything the
/// algorithm computes, so the atomics themselves need no more than `Relaxed`.
pub struct Entity {
    /// The virtual runtime, in virtual nanoseconds, weighted by
    /// [`NICE_0_LOAD`] over the task's weight.
    ///
    /// A task that is queued never has this changed, which is what lets a
    /// runqueue order queued entities by the deadline they were queued with.
    vruntime: AtomicU64,
    /// The task's weight, which is a function of its nice value.
    weight: AtomicU32,
    /// The time slice the task asked for, in real nanoseconds.
    ///
    /// Fixed for the life of the entity, so a runqueue may read it while the
    /// task is queued without the position of the task changing underneath it.
    slice: AtomicU64,
    /// The order in which this entity was created, used to break ties.
    seq: AtomicU32,
    /// The time the task has spent running, in real nanoseconds.
    exec_ns: AtomicU64,
    /// The time the task has spent waiting on a runqueue, in real nanoseconds.
    rq_wait_ns: AtomicU64,
    /// The number of times the task has been dispatched to a CPU.
    nr_switches: AtomicU64,
    /// The number of times the task has been woken on a different CPU than the
    /// one it last ran on, which stays zero until there is load balancing.
    nr_migrations: AtomicU64,
    /// The tick at which the task was last dispatched or last charged.
    last_run: AtomicU64,
    /// Whether a runqueue has ever placed this task, which decides whether the
    /// task is a newcomer that is owed nothing or one that is owed the lag it
    /// was last seen with.
    placed: AtomicBool,
    /// The lag the task was last seen with, kept while it is off the runqueue so
    /// that it comes back to the same place.
    ///
    /// Positive is owed CPU, negative has had too much of it. This is in the
    /// task's own virtual time, so a task of large weight, whose virtual clock
    /// runs slowly, keeps a lag that is worth more real time.
    lag: AtomicI64,
}

impl core::fmt::Debug for Entity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The atomics are read one by one, so this is a snapshot that may be
        // slightly inconsistent. That is fine for a debug dump.
        f.debug_struct("Entity")
            .field("vruntime", &self.vruntime())
            .field("weight", &self.weight())
            .field("slice_ns", &self.slice_ns())
            .finish_non_exhaustive()
    }
}

impl Entity {
    /// Creates an entity for a task of nice value `nice` that wants the
    /// default time slice.
    pub fn new(nice: i8) -> Arc<Self> {
        Self::with_options(nice, BASE_SLICE_NS)
    }

    /// Creates an entity for a task of nice value `nice` that wants a time
    /// slice of `slice_ns` nanoseconds.
    pub fn with_options(nice: i8, slice_ns: u64) -> Arc<Self> {
        Arc::new(Self {
            vruntime: AtomicU64::new(0),
            weight: AtomicU32::new(weight_from_nice(nice)),
            slice: AtomicU64::new(slice_ns.clamp(MIN_SLICE_NS, MAX_SLICE_NS)),
            seq: AtomicU32::new(NEXT_SEQ.fetch_add(1, Ordering::Relaxed)),
            exec_ns: AtomicU64::new(0),
            rq_wait_ns: AtomicU64::new(0),
            nr_switches: AtomicU64::new(0),
            nr_migrations: AtomicU64::new(0),
            last_run: AtomicU64::new(0),
            placed: AtomicBool::new(false),
            lag: AtomicI64::new(0),
        })
    }

    /// Creates the state of the idle task of a CPU.
    ///
    /// The idle task has a weight of zero, which is how the runqueue knows to
    /// leave it out of the weighted mean: it is not competing for anything. It
    /// is never enqueued either, and a task with a weight of zero never has its
    /// virtual runtime advanced, so it can never be mistaken for a task that
    /// is owed CPU.
    pub fn idle() -> Arc<Self> {
        Arc::new(Self {
            vruntime: AtomicU64::new(0),
            weight: AtomicU32::new(0),
            slice: AtomicU64::new(BASE_SLICE_NS),
            seq: AtomicU32::new(NEXT_SEQ.fetch_add(1, Ordering::Relaxed)),
            exec_ns: AtomicU64::new(0),
            rq_wait_ns: AtomicU64::new(0),
            nr_switches: AtomicU64::new(0),
            nr_migrations: AtomicU64::new(0),
            last_run: AtomicU64::new(0),
            // The idle task is installed rather than placed, so it is never a
            // newcomer and never keeps the lag of a sleep.
            placed: AtomicBool::new(true),
            lag: AtomicI64::new(0),
        })
    }

    /// Returns the task's virtual runtime, in virtual nanoseconds.
    pub fn vruntime(&self) -> u64 {
        self.vruntime.load(Ordering::Relaxed)
    }

    /// Sets the task's virtual runtime.
    ///
    /// Only a runqueue that owns the entity may call this, and only while the
    /// entity is not queued, because a queued entity's position in a runqueue
    /// is derived from its virtual runtime.
    pub fn set_vruntime(&self, vruntime: u64) {
        self.vruntime.store(vruntime, Ordering::Relaxed);
    }

    /// Returns the task's weight.
    pub fn weight(&self) -> u32 {
        self.weight.load(Ordering::Relaxed)
    }

    /// Returns the nice value that corresponds to the task's weight.
    pub fn nice(&self) -> i8 {
        nice_from_weight(self.weight())
    }

    /// Returns the task's time slice, in real nanoseconds.
    pub fn slice_ns(&self) -> u64 {
        self.slice.load(Ordering::Relaxed)
    }

    /// Returns the tie-breaking sequence number of the entity.
    ///
    /// The first task created has sequence number 0.
    pub fn seq(&self) -> u32 {
        self.seq.load(Ordering::Relaxed)
    }

    /// Returns the number of virtual nanoseconds the task's time slice is
    /// worth, which is the size of its request.
    ///
    /// This is the task's deadline minus its virtual runtime, so a task of
    /// large weight gets a small request and therefore a small shift between
    /// its virtual runtime and its deadline. Two tasks of the same weight that
    /// asked for the same slice are a whole request apart in their deadlines
    /// however different their virtual runtimes are, which is what makes the
    /// choice between them a choice about fair share rather than about
    /// priority.
    ///
    /// A task of weight zero is the idle task, which is not competing for
    /// anything and has no request.
    pub fn request(&self) -> u64 {
        let weight = self.weight() as u64;
        if weight == 0 {
            return 0;
        }
        scale(self.slice_ns(), NICE_0_LOAD as u64, weight)
    }

    /// Returns the task's virtual deadline, in virtual nanoseconds: the point
    /// in virtual time by which it wants to have finished its request.
    pub fn deadline(&self) -> u64 {
        self.vruntime().saturating_add(self.request())
    }

    /// Charges the task `delta_ns` of real time on a CPU.
    ///
    /// The virtual runtime advances in proportion to the weight, so the task's
    /// lag against its peers stays put.
    pub fn charge(&self, delta_ns: u64) {
        self.exec_ns.fetch_add(delta_ns, Ordering::Relaxed);
        let weight = self.weight() as u64;
        if weight == 0 {
            // The idle task does not compete for CPU, so it has no virtual
            // runtime to advance. Dividing by its weight would not have a
            // defined answer anyway.
            return;
        }
        let delta = scale(delta_ns, NICE_0_LOAD as u64, weight);
        // A run of charges that saturates the virtual runtime rather than
        // wrapping it around would leave the task eligible for ever, so the
        // add has to saturate.
        let mut vruntime = self.vruntime();
        while vruntime != u64::MAX {
            match self.vruntime.compare_exchange_weak(
                vruntime,
                vruntime.saturating_add(delta),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => vruntime = actual,
            }
        }
    }

    /// Records that the task was dispatched to a CPU at tick `now` after
    /// having been waiting since tick `arrived`.
    pub fn dispatched(&self, now: u64, arrived: u64) {
        self.nr_switches.fetch_add(1, Ordering::Relaxed);
        self.last_run.store(now, Ordering::Relaxed);
        self.rq_wait_ns.fetch_add(
            now.saturating_sub(arrived).saturating_mul(NSEC_PER_TICK),
            Ordering::Relaxed,
        );
    }

    /// Returns the tick at which the task was last dispatched or charged.
    pub fn last_run(&self) -> u64 {
        self.last_run.load(Ordering::Relaxed)
    }

    /// Returns whether a runqueue has ever placed this task.
    pub fn is_placed(&self) -> bool {
        self.placed.load(Ordering::Relaxed)
    }

    /// Records that a runqueue has placed this task.
    pub fn set_placed(&self) {
        self.placed.store(true, Ordering::Relaxed);
    }

    /// Returns the lag the task was last seen with, in virtual nanoseconds.
    pub fn lag(&self) -> i64 {
        self.lag.load(Ordering::Relaxed)
    }

    /// Records the lag the task was last seen with.
    pub fn set_lag(&self, lag: i64) {
        self.lag.store(lag, Ordering::Relaxed);
    }

    /// Returns a snapshot of the task's statistics.
    pub fn stats(&self) -> TaskStats {
        TaskStats {
            nice: self.nice(),
            weight: self.weight(),
            slice_ns: self.slice_ns(),
            vruntime: self.vruntime(),
            deadline: self.deadline(),
            exec_ns: self.exec_ns.load(Ordering::Relaxed),
            rq_wait_ns: self.rq_wait_ns.load(Ordering::Relaxed),
            nr_switches: self.nr_switches.load(Ordering::Relaxed),
            nr_migrations: self.nr_migrations.load(Ordering::Relaxed),
        }
    }
}

/// Returns `value * numerator / denominator`, rounded down and saturating.
///
/// The multiplication is done on 128 bits where the target has them so that a
/// large `value` cannot wrap, and a plain `u64` elsewhere.
#[cfg(target_pointer_width = "64")]
fn scale(value: u64, numerator: u64, denominator: u64) -> u64 {
    u64::try_from((value as u128 * numerator as u128) / denominator as u128).unwrap_or(u64::MAX)
}

/// Returns `value * numerator / denominator`, rounded down and saturating.
#[cfg(not(target_pointer_width = "64"))]
fn scale(value: u64, numerator: u64, denominator: u64) -> u64 {
    value
        .saturating_mul(numerator)
        .checked_div(denominator)
        .unwrap_or(0)
}
