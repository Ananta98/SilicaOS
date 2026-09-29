// SPDX-License-Identifier: GPL-2.0

//! Task scheduling.
//!
//! The scheduler is the earliest eligible virtual deadline first (EEVDF)
//! algorithm, which is what Linux has used since 6.6. OSTD has no scheduler of
//! its own: it defines the [`Scheduler`](ostd::task::scheduler::Scheduler) and
//! [`LocalRunQueue`](ostd::task::scheduler::LocalRunQueue) traits and calls into
//! whatever is injected, so this module is that implementation.
//!
//! # What EEVDF is
//!
//! Two tasks of the same weight should each get half of a CPU they contend for,
//! two tasks of different weights should get shares in proportion, and a task
//! that wants little latency should get it without giving up its share. EEVDF
//! gets all three out of one rule.
//!
//! Every task has a **virtual runtime**: the CPU time it has received, counted
//! at a rate inversely proportional to its weight, so that a task twice as heavy
//! as another advances its virtual runtime at half the rate and reaches the same
//! virtual time after twice the real time. The **virtual time** of a runqueue is
//! the weighted mean of the virtual runtimes of the tasks on it, and the
//! difference between a task's own virtual runtime and that mean is its **lag**:
//! positive lag means it has received less than its share and is owed CPU,
//! negative lag that it has received more. The lags of a runqueue always cancel
//! out, because the mean is a mean.
//!
//! - A task whose lag is not negative is **eligible**: it has not had more than
//!   its share.
//! - A task's **virtual deadline** is the point in virtual time by which it
//!   wants to have finished its request, which is its own virtual runtime plus
//!   the virtual time its requested time slice is worth. A heavier task's slice
//!   is worth less of its own virtual time, so the request is what separates
//!   tasks of different weight rather than the deadline.
//!
//! The rule is then: *run the eligible task with the earliest virtual deadline*.
//!
//! Fairness is the eligibility test. A task that has had its share stops being
//! eligible and so stops being chosen, which is also what ends a turn of
//! scheduling without any code watching a slice expire; what ends a turn of
//! *running* is the clock, since a decision is only taken when the clock ticks or
//! a task yields or blocks. Latency is the deadline: a task that wants a short
//! slice asks for a short request, gets a deadline that much earlier than
//! everyone else's, and is run next, and may interrupt a task that has not
//! finished a longer request, without having given up any of its share. That is
//! what EEVDF bought over the "smallest virtual runtime first" rule it
//! replaced, which could not tell a task that wanted 100 microseconds of
//! response from one that wanted 100 milliseconds.
//!
//! # What the modules are
//!
//! - `entity` — the state of one task: its virtual runtime, weight, slice, lag
//!   and statistics.
//! - `rq` — a CPU's runqueue, which is where the algorithm is.
//! - `adapter` — the [`Scheduler`](ostd::task::scheduler::Scheduler) that OSTD
//!   drives, the registry that gives a task its state, and the idle task.
//! - [`loadavg`] — the load average.
//! - [`stats`] — what the scheduler knows about how the system is running.
//! - [`selftest`] — a self test of the whole, for when the scheduler is being
//!   brought up.
//!
//! # What is not here yet
//!
//! One runqueue per CPU exists and a task is put on the CPU that woke it, but
//! nothing moves work between CPUs: with the one CPU the machine boots with,
//! there is nothing to balance, and load balancing is a piece of work of its
//! own. There are no real-time classes, and a task's weight is fixed when it is
//! created, so there is no `setpriority` to change it — which is why
//! [`weight_from_nice`] and [`Entity::with_options`] take a nice value and a
//! slice rather than the other way round.
//!
//! # The clock
//!
//! Everything the scheduler measures is in ticks of the system timer, which is
//! [`TIMER_HZ`] hertz, so a scheduling decision is quantised to a millisecond
//! and the shares a task gets are quantised with it. The accounting itself is
//! finer than that: virtual runtimes are counted in nanoseconds of virtual time
//! and a charge converts the ticks, because a task of the largest weight
//! advances its virtual runtime by well under a millisecond a tick and a whole
//! millisecond would be more than all of it. A cycle counter calibrated against
//! the timer would sharpen the decisions as well, and `now` is the one place that
//! would have to change.
//! 
pub mod adapter;
pub mod entity;
pub mod loadavg;
pub mod rq;
pub mod stats;

use ostd::timer::{Jiffies, TIMER_FREQ};

pub use self::{
    adapter::{
        build, build_with_slice, entity_of, init, init_on_cpu, is_system_idle, nr_running_on,
        spawn, spawn_with_slice, stats_of_cpu,
    },
    entity::{
        BASE_SLICE_NS, Entity, MAX_SLICE_NS, MIN_SLICE_NS, NICE_0_LOAD, NICE_MAX, NICE_MIN,
        weight_from_nice,
    },
    loadavg::{LoadAvg, loadavg},
    stats::{CpuStatsSnapshot, GlobalStatsSnapshot, TaskStats},
};

/// The number of ticks the system timer fires per second.
pub const TIMER_HZ: u64 = TIMER_FREQ;

/// The length of one tick of the system timer, in nanoseconds.
pub const NSEC_PER_TICK: u64 = 1_000_000_000 / TIMER_HZ;

/// Returns the number of timer ticks since the system booted.
///
/// Every decision the scheduler takes is taken on this clock, and so is all the
/// accounting; see the module documentation for what a tick costs in precision.
pub(crate) fn now() -> u64 {
    Jiffies::elapsed().as_u64()
}
