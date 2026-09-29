// SPDX-License-Identifier: GPL-2.0

//! A CPU's runqueue, which is where the EEVDF algorithm is implemented.
//!
//! A [`RunQueue`] holds every task that is runnable on one CPU: the one running
//! now in `current`, and the rest in `queued`. Together they form the set whose
//! weighted mean virtual runtime is the runqueue's virtual time, which is the
//! reference point every task's lag is measured against.
//!
//! # Choosing the next task
//!
//! [`RunQueue::best`] walks `queued` in increasing virtual deadline and returns
//! the first task that is eligible. Because the walk never looks back, that is
//! the eligible task with the earliest deadline of all, with no need to sort or
//! to search the whole queue for the best of them.
//!
//! The running task competes in the same comparison, so it keeps running for as
//! long as it stays eligible, which is until it has had its share of the CPU.
//! Nothing has to watch a slice expire to end a turn, because a turn ends when a
//! task has had enough rather than when a timer says so; what a turn is
//! measured in is the clock, since that is the only thing that makes a decision
//! possible. A task's slice does decide something: it sets how far ahead of its
//! virtual runtime the task's deadline is, and so how much CPU it may take before
//! a woken task with an earlier deadline may interrupt it.
//!
//! # Keeping the virtual time cheap
//!
//! The weighted mean is kept as two running sums: `sum_weight` and
//! `sum_offset_weighted`, the latter counting `(vruntime - min_vruntime) *
//! weight`. Measuring against `min_vruntime` rather than zero is what keeps
//! those sums inside a `u64`. Virtual runtimes themselves grow for as long as
//! the system is up, whereas the spread between two of them on one runqueue is
//! bounded by the size of a request, so the difference is the quantity that
//! stays small. `min_vruntime` is the smallest virtual runtime in the runqueue
//! and only ever moves forward.
//!
//! # Locking
//!
//! A runqueue belongs to one CPU and may only be touched while that CPU's lock
//! is held, so nothing in here needs to be atomic except the counters in
//! [`stats`](crate::sched::stats), which the statistics readers take the lock
//! to read.
//!
//! The only lock this module nests is the runqueue lock, and the scheduler's
//! entity registry, always in that order: the registry is looked up and
//! released before a runqueue is locked, and only ever taken again while a
//! runqueue is held.

use alloc::{collections::BTreeMap, sync::Arc};

use ostd::task::Task;

use ostd::{cpu::CpuId, task::scheduler::UpdateFlags};

use super::{Entity, NSEC_PER_TICK, now, stats::CpuStats};

/// A runnable task together with the scheduling state that places it in a
/// runqueue, and the tick at which it joined that runqueue.
///
/// The arrival tick is per runqueue rather than per task because a task woken
/// on one CPU and run on another is queued twice, and the time spent waiting
/// for a CPU is a property of a wait, not of a task.
#[derive(Clone)]
pub struct Entry {
    /// The task.
    pub(crate) task: Arc<Task>,
    /// The task's scheduling state.
    pub(crate) entity: Arc<Entity>,
    /// The tick at which the task joined this runqueue, which is when its
    /// wait for the CPU started.
    pub(crate) arrival: u64,
    /// Whether this is the runqueue's idle task, which runs only when nothing
    /// else can.
    pub(crate) is_idle: bool,
}

impl Entry {
    /// Creates an entry for `task` that joined the runqueue at tick `arrival`.
    pub fn new(task: Arc<Task>, entity: Arc<Entity>, arrival: u64) -> Self {
        Self {
            task,
            entity,
            arrival,
            is_idle: false,
        }
    }

    /// Creates the entry of the idle task of a CPU.
    ///
    /// The idle task is not enqueued: it is installed as the running task
    /// directly, so it must never be woken, and nothing may be scheduled in its
    /// place. That is what its weight of zero and the runqueue's exclusion of
    /// it are for.
    pub fn idle(task: Arc<Task>, entity: Arc<Entity>) -> Self {
        Self {
            task,
            entity,
            arrival: now(),
            is_idle: true,
        }
    }

    /// Returns the task.
    pub fn into_task(self) -> Arc<Task> {
        self.task
    }

    /// Returns a reference to the task.
    pub fn task(&self) -> &Arc<Task> {
        &self.task
    }

    /// Returns a reference to the entity.
    pub fn entity(&self) -> &Arc<Entity> {
        &self.entity
    }
}

/// The position of a task in a runqueue.
///
/// A runqueue is ordered by virtual deadline so that [`RunQueue::best`] can
/// stop at the first eligible task. Ties on the deadline go to the smaller
/// virtual runtime, and remaining ties to the sequence number, which makes the
/// choice deterministic even between tasks that are otherwise
/// indistinguishable.
///
/// All three fields are constant for as long as a task is in `queued`, which is
/// what lets a runqueue find a queued task by the position it was placed at: a
/// queued task's virtual runtime is never charged, and its weight and slice are
/// fixed for the life of the task.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RunKey {
    /// The task's virtual deadline.
    deadline: u64,
    /// The task's virtual runtime, which breaks deadline ties in favour of
    /// whoever has run less.
    vruntime: u64,
    /// The task's sequence number, which breaks the remaining ties.
    seq: u32,
}

impl RunKey {
    /// Returns the position of `entity`, with its deadline pushed one request
    /// further out if `yielding`.
    ///
    /// The push is how a task that asked to be switched away from loses its
    /// place: it is treated as if it had asked for one more request, which
    /// puts it behind everything that is waiting with a request of its own. It
    /// only affects the comparison, so a yield that changes nothing leaves the
    /// task exactly where it was.
    fn of(entity: &Entity, yielding: bool) -> Self {
        let deadline = entity.deadline();
        Self {
            deadline: deadline.saturating_add(if yielding {
                entity.request()
            } else {
                0
            }),
            vruntime: entity.vruntime(),
            seq: entity.seq(),
        }
    }
}

/// Returns whether `entity` may run while the runqueue's virtual time is
/// `avg_vruntime`.
///
/// A task is eligible while it has not received more than its share, which is
/// while its virtual runtime has not passed the runqueue's.
fn is_eligible(entity: &Entity, avg_vruntime: u64) -> bool {
    entity.vruntime() <= avg_vruntime
}

/// The runqueue of one CPU.
pub struct RunQueue {
    /// The task running on this CPU, or nothing at all while no task runs,
    /// which is the case in the bootstrap context.
    current: Option<Entry>,
    /// The runnable tasks waiting for the CPU, in order of virtual deadline.
    queued: BTreeMap<RunKey, Entry>,
    /// The sum of the weights of `current` and `queued`.
    sum_weight: u64,
    /// The sum of `(vruntime - min_vruntime) * weight` over `current` and
    /// `queued`.
    sum_offset_weighted: u64,
    /// The smallest virtual runtime in `current` and `queued`, which only ever
    /// moves forward.
    min_vruntime: u64,
    /// The tick at which the running task was last charged.
    last_update: u64,
    /// The idle task of this CPU, kept even while it is not running so that it
    /// can be run again.
    idle: Option<Entry>,
    /// Whether a task has ever been enqueued here, which tells a CPU that has
    /// work to do from one that never had any.
    has_run_tasks: bool,
    /// The event that last asked for a scheduling decision, which decides
    /// whether a pending yield counts and whether the task being removed is
    /// exiting.
    last_flags: UpdateFlags,
    /// This CPU's statistics.
    stats: CpuStats,
}

impl RunQueue {
    /// Creates an empty runqueue.
    pub const fn new() -> Self {
        Self {
            current: None,
            queued: BTreeMap::new(),
            sum_weight: 0,
            sum_offset_weighted: 0,
            min_vruntime: 0,
            last_update: 0,
            idle: None,
            has_run_tasks: false,
            last_flags: UpdateFlags::Tick,
            stats: CpuStats::new(),
        }
    }

    /// Returns the task running on this CPU, if any.
    pub fn current_task(&self) -> Option<&Arc<Task>> {
        self.current.as_ref().map(|entry| &entry.task)
    }

    /// Returns the scheduling state of the task running on this CPU, if any.
    #[cfg(ktest)]
    pub fn current_entity(&self) -> Option<&Arc<Entity>> {
        self.current.as_ref().map(|entry| &entry.entity)
    }

    /// Returns the number of runnable tasks on this CPU.
    ///
    /// The idle task is not runnable: it runs because there is nothing else to
    /// run, which is not the same as there being work. Neither is the bootstrap
    /// context, which is not a task at all.
    ///
    /// This is what the load average counts.
    pub fn nr_running(&self) -> usize {
        self.queued.len()
            + usize::from(self.current.as_ref().is_some_and(|entry| !entry.is_idle))
    }

    /// Returns the number of tasks waiting for the CPU, the running task
    /// excluded.
    pub fn nr_queued(&self) -> usize {
        self.queued.len()
    }

    /// Returns this CPU's statistics.
    pub fn stats(&self) -> &CpuStats {
        &self.stats
    }

    /// Returns the runqueue's virtual time: the weighted mean of the virtual
    /// runtimes of its runnable tasks.
    ///
    /// A task's lag is the difference between this and its own virtual runtime,
    /// so a task below this point has received less than its share and a task
    /// above it has received more.
    pub fn avg_vruntime(&self) -> u64 {
        if self.sum_weight == 0 {
            return self.min_vruntime;
        }
        self.min_vruntime
            .saturating_add(self.sum_offset_weighted / self.sum_weight)
    }

    /// Returns the smallest virtual runtime in the runqueue.
    pub fn min_vruntime(&self) -> u64 {
        self.min_vruntime
    }

    /// Returns whether the CPU is running its idle task.
    pub fn is_idling(&self) -> bool {
        self.current.as_ref().is_some_and(|entry| entry.is_idle)
    }

    /// Charges the running task for the time since it was last charged, so that
    /// its virtual runtime and this CPU's accounting are up to date before any
    /// decision is taken.
    pub fn charge(&mut self) {
        let now = now();
        if self.current.is_none() {
            // Nothing is running, so there is nothing to charge, but the clock
            // still has to be noted for the next task that is dispatched.
            self.last_update = now;
            return;
        }
        self.charge_until(now);
    }

    /// Charges the running task for the time that has passed since `at`, which
    /// is where the caller last left the clock.
    pub fn charge_until(&mut self, at: u64) {
        let delta = at.saturating_sub(self.last_update);
        self.last_update = at;
        self.charge_delta(delta);
    }

    /// Charges the running task `delta` ticks of time.
    ///
    /// A task of large weight advances its virtual runtime more slowly, so this
    /// is where a task is given the share of the CPU its weight earns it.
    ///
    /// The charge is made in nanoseconds rather than in ticks because the clock
    /// ticks once a millisecond and a task of the largest weight advances its
    /// virtual runtime by only `1024 / 88761` of a tick, which is nothing at all
    /// in whole ticks. Nanoseconds leave the step of a heavy task at tens of
    /// thousands, so no task is ever charged nothing for a tick of running. It
    /// is the same conversion that puts the time on a CPU into the statistics
    /// being reported in nanoseconds.
    pub fn charge_delta(&mut self, delta: u64) {
        if delta == 0 {
            return;
        }
        let Some(current) = &self.current else {
            return;
        };

        let before = current.entity.vruntime();
        let delta_ns = delta.saturating_mul(NSEC_PER_TICK);
        current.entity.charge(delta_ns);
        self.sum_offset_weighted = self.sum_offset_weighted.saturating_add(
            current
                .entity
                .vruntime()
                .saturating_sub(before)
                .saturating_mul(current.entity.weight() as u64),
        );
        if current.is_idle {
            self.stats.idled(delta_ns);
        } else {
            self.stats.ran(delta_ns);
        }
        if before == self.min_vruntime {
            self.advance_min_vruntime();
        }
    }

    /// Places `entry` in the runqueue at the position its lag earns it.
    ///
    /// A task that is coming back is given the lag it was last seen with, which
    /// is what it takes for a task that gives up the CPU to get it back and what
    /// it takes for a task that sleeps to be treated as it was rather than as a
    /// newcomer. The lag is bounded, so a task that slept for a minute does not
    /// come back owed a minute of CPU: it comes back owed what it was owed when
    /// it went to sleep, which is at most a couple of requests, and that is what
    /// keeps a task from gaming the scheduler by sleeping at the end of a good
    /// turn to escape a debt.
    ///
    /// A task the scheduler has not seen before is given a lag of zero, the
    /// neutral position: it is eligible, but it queues behind the tasks that are
    /// already behind.
    pub fn place(&mut self, entry: Entry, is_new: bool) {
        let lag = if is_new { 0 } else { entry.entity.lag() };
        // Adding a task moves the runqueue's virtual time, because the virtual
        // time is the mean of the tasks on it and there is one more of them now:
        // a task that is owed CPU is placed below the mean and raises it, and a
        // task that has had too much is placed above and lowers it. So the lag a
        // task comes back with has to be scaled by the change in the total
        // weight, or it would come back with a different lag from the one it
        // left with. This is what Linux does when it places an entity; the
        // arithmetic is the solution of `V_after = mean(V_before, v)` for `v`.
        let weight = entry.entity.weight() as i64;
        let total = self.sum_weight as i64;
        let scaled_lag = if total == 0 || weight == 0 {
            // Nothing to be late for, or a task that is not in the mean at all.
            lag
        } else {
            ((lag as i128 * (total as i128 + weight as i128)) / total as i128) as i64
        };
        let base_vruntime = self.avg_vruntime();
        let vruntime = (base_vruntime as i128 - scaled_lag as i128).max(0) as u64;

        if self.sum_weight == 0 {
            self.min_vruntime = vruntime;
            entry.entity.set_vruntime(vruntime);
        } else {
            entry
                .entity
                .set_vruntime(vruntime.max(self.min_vruntime));
        }
        entry.entity.set_placed();
        self.insert(entry, false);
        self.has_run_tasks = true;
    }

    /// Returns the task that should run, or `None` if the running task should
    /// keep running.
    ///
    /// `None` is also the right answer when the runqueue holds nothing but the
    /// running task, which is what keeps a yield of the only runnable task from
    /// switching to itself.
    pub fn pick_next(&self, yielding: bool) -> Option<Entry> {
        let best = self.best()?;
        self.wins_over_current(best, yielding)
            .then(|| best.clone())
    }

    /// Returns the queued task with the earliest virtual deadline among those
    /// that are eligible.
    fn best(&self) -> Option<&Entry> {
        if self.queued.is_empty() {
            return None;
        }
        let avg_vruntime = self.avg_vruntime();
        // The queue is ordered by deadline, so the first eligible task found is
        // the eligible task with the earliest deadline. Rounding in the
        // weighted mean can in principle leave nothing eligible, and a
        // runqueue with nothing eligible must still run something, so the
        // earliest deadline stands in.
        self.queued
            .iter()
            .find(|(_, entry)| is_eligible(&entry.entity, avg_vruntime))
            .map(|(_, entry)| entry)
            .or_else(|| self.queued.values().next())
    }

    /// Returns whether the CPU could start idling if it had nothing else to
    /// run, which is what makes giving up the CPU worth a switch.
    pub fn can_idle(&self) -> bool {
        self.has_run_tasks && !self.is_idling()
    }

    /// Returns whether `candidate` should displace the running task.
    fn wins_over_current(&self, candidate: &Entry, yielding: bool) -> bool {
        let Some(current) = &self.current else {
            return true;
        };
        // The idle task gives way to anything, and so does a task that has
        // received more than its share.
        if current.is_idle || !is_eligible(&current.entity, self.avg_vruntime()) {
            return true;
        }
        RunKey::of(&candidate.entity, false) < RunKey::of(&current.entity, yielding)
    }

    /// Returns whether switching away from the running task would pay off,
    /// which is what the scheduler asks on every timer tick.
    pub fn should_switch(&self) -> bool {
        if self.current.is_none() {
            // There is no running task to switch away from. This is the
            // bootstrap context, which no task can switch back to, so it is
            // never preempted.
            return false;
        }
        self.best().is_some_and(|best| self.wins_over_current(best, false))
    }

    /// Returns whether the task of `entity`, which has just been enqueued, is
    /// worth interrupting the running task for.
    ///
    /// This is the same comparison a wakeup would face a moment later, so a
    /// task that asks for a preemption here will also be picked when the tick
    /// arrives. Only a task that is eligible can, because fairness comes
    /// before urgency.
    pub fn should_preempt_entity(&self, entity: &Entity) -> bool {
        let Some(current) = &self.current else {
            // Nothing is running, and the context that is running is one that
            // cannot be switched back to.
            return false;
        };
        if current.is_idle {
            return true;
        }
        is_eligible(entity, self.avg_vruntime())
            && RunKey::of(entity, false) < RunKey::of(&current.entity, false)
    }

    /// Makes `next` the running task, putting the running task back into the
    /// runqueue unless it is the idle task.
    ///
    /// `from_queue` says whether `next` is in `queued`, which it is not when it
    /// is the idle task being started.
    ///
    /// The caller must have charged the runqueue with [`RunQueue::charge`]
    /// first, because the outgoing task's accounting is folded into the switch
    /// it is about to make.
    pub fn install(&mut self, next: Entry, from_queue: bool) {
        let next = if from_queue {
            let key = RunKey::of(&next.entity, false);
            self.queued.remove(&key).unwrap_or_else(|| {
                // The runqueue lock is held for the whole decision, so a
                // task cannot leave the queue underneath it. Switching to the
                // copy in hand anyway would leave the queue holding a task
                // that is running, which would be far worse than a panic.
                panic!("the picked task is not in the runqueue");
            })
        } else {
            // The idle task is never queued and never counted: a task of weight
            // zero is left out of the weighted mean, which is exactly what an
            // idle task should be, since it competes for nothing.
            next
        };

        // Neither task moves in or out of the running sums: one was queued and
        // is now running, the other was running and is now queued.
        if let Some(mut current) = self.current.replace(next) {
            // Only a task that was running is a switch away from something, so
            // the first task a CPU runs is a dispatch and not a context switch.
            self.stats.switched_out(self.last_flags == UpdateFlags::Yield);
            super::stats::global().switched();
            if !current.is_idle {
                current.arrival = now();
                self.insert(current, true);
            }
        }

        let running = self.current.as_ref().expect("just replaced");
        running.entity.dispatched(now(), running.arrival);
    }

    /// Removes the running task from the runqueue and returns it.
    ///
    /// The idle task is not removed: a runqueue holding nothing but its idle
    /// task has to keep it, because the idle task is the only thing that can
    /// wait for an interrupt.
    pub fn dequeue_current(&mut self) -> Option<Entry> {
        if self.is_idling() {
            return None;
        }
        let current = self.current.take()?;
        // The record of which CPU the task is on is what says whether it is on a
        // runqueue at all, so clearing it here is what lets a later wake of this
        // task put it back on one instead of being taken for a wake of a task
        // that is already queued.
        current.task.schedule_info().cpu.set_to_none();
        self.remember_lag(&current.entity);
        self.unaccount(&current.entity);
        if self.last_flags == UpdateFlags::Exit {
            // The task is exiting and will never be enqueued again, so its
            // scheduling state is not worth keeping.
            super::adapter::forget_entity(&current.task);
        }
        Some(current)
    }

    /// Starts idling this CPU if it has work to wait for and nothing to run.
    ///
    /// Returns whether the CPU is running the idle task afterwards.
    pub fn start_idling(&mut self) -> bool {
        if self.is_idling() {
            return true;
        }
        if !self.has_run_tasks {
            // This runqueue has never held a task, so there is nothing to wait
            // for. Saying so leaves the CPU with no running task at all, which
            // is what lets the bootstrap context finish and the machine power
            // off.
            return false;
        }
        if self.idle.is_none() {
            // The idle task is built here, under the runqueue lock, rather than
            // when the CPU starts up, so that a kernel that never runs out of
            // tasks never pays for one. Allocating a kernel stack is a few
            // atomic operations on a free list, which is a fair price for that.
            let Some(idle) = super::adapter::idle_entry() else {
                return false;
            };
            self.idle = Some(idle.clone());
        }
        let idle = self.idle.clone().expect("just created");
        self.install(idle, false);
        true
    }

    /// Returns the last event that asked for a scheduling decision.
    pub fn last_flags(&self) -> UpdateFlags {
        self.last_flags
    }

    /// Records the event that asked for a scheduling decision.
    pub fn set_last_flags(&mut self, flags: UpdateFlags) {
        self.last_flags = flags;
    }

    /// Returns the tasks waiting for the CPU, earliest deadline first.
    pub fn queued_tasks(&self) -> impl Iterator<Item = (&Arc<Task>, &Arc<Entity>)> {
        self.queued
            .values()
            .map(|entry| (&entry.task, &entry.entity))
    }

    /// Writes what this runqueue holds to the kernel log.
    ///
    /// For bringing a scheduler up: where the runqueue's virtual time is, what
    /// the running task has been given, and what every waiting task is owed.
    pub(super) fn log(&self, cpu: CpuId) {
        ostd::info!(
            "  cpu{}: {} runnable, {} waiting, virtual time {} (smallest {})",
            u32::from(cpu),
            self.nr_running(),
            self.nr_queued(),
            self.avg_vruntime(),
            self.min_vruntime(),
        );
        match &self.current {
            Some(current) => ostd::info!(
                "    running {}: {}",
                if current.is_idle { "the idle task" } else { "a task" },
                current.entity.stats(),
            ),
            None => ostd::info!("    running: no task"),
        }
        for (task, entity) in self.queued_tasks() {
            ostd::info!("    waiting {:p}: {}", Arc::as_ptr(task), entity.stats());
        }
    }

    /// Puts `entry` into the queue at the position its deadline gives it.
    ///
    /// `counted` says whether the task is already in the running sums, which is
    /// the case for a task that is being switched away from and a task that is
    /// being switched to, and is not the case for a task that is arriving.
    fn insert(&mut self, entry: Entry, counted: bool) {
        let key = RunKey::of(&entry.entity, false);
        let entity = Arc::clone(&entry.entity);
        let previous = self.queued.insert(key, entry);
        debug_assert!(previous.is_none(), "a task is queued only once");
        if !counted {
            self.account(&entity);
        }
    }

    /// Records the lag `entity` leaves the runqueue with.
    ///
    /// The lag is bounded by a couple of requests, which is the bound Linux
    /// uses: without it a task could be owed an unbounded amount of CPU simply
    /// by going to sleep while the runqueue's virtual time ran on, and then take
    /// all of it back the moment it woke up.
    fn remember_lag(&self, entity: &Entity) {
        let lag = self.avg_vruntime() as i64 - entity.vruntime() as i64;
        let limit = 2 * entity.request() as i64;
        entity.set_lag(lag.clamp(-limit, limit));
    }

    /// Adds `entity` to the running sums.
    fn account(&mut self, entity: &Entity) {
        let weight = entity.weight() as u64;
        self.sum_weight = self.sum_weight.saturating_add(weight);
        self.sum_offset_weighted = self.sum_offset_weighted.saturating_add(
            entity
                .vruntime()
                .saturating_sub(self.min_vruntime)
                .saturating_mul(weight),
        );
    }

    /// Removes `entity` from the running sums, and moves `min_vruntime` up if
    /// it was the one that left.
    fn unaccount(&mut self, entity: &Entity) {
        let weight = entity.weight() as u64;
        let vruntime = entity.vruntime().saturating_sub(self.min_vruntime);
        self.sum_weight = self.sum_weight.saturating_sub(weight);
        self.sum_offset_weighted = self
            .sum_offset_weighted
            .saturating_sub(vruntime.saturating_mul(weight));
        if vruntime == 0 {
            self.advance_min_vruntime();
        }
    }

    /// Moves `min_vruntime` up to the smallest virtual runtime left in the
    /// runqueue, if the smallest one has left.
    fn advance_min_vruntime(&mut self) {
        let Some(smallest) = self
            .current
            .iter()
            .chain(self.queued.values())
            .map(|entry| entry.entity.vruntime())
            .min()
        else {
            return;
        };
        let advance = smallest.saturating_sub(self.min_vruntime);
        if advance == 0 {
            return;
        }
        self.min_vruntime = smallest;
        // Every offset shrinks by the same amount, so the weighted sum does
        // too. This is what keeps the sums from overflowing on a long run.
        self.sum_offset_weighted = self
            .sum_offset_weighted
            .saturating_sub(advance.saturating_mul(self.sum_weight));
    }
}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new()
    }
}
#[cfg(ktest)]
mod tests {
    use alloc::{sync::Arc, vec::Vec};

    use ostd::{prelude::ktest, task::TaskOptions};

    use super::*;
    use crate::sched::{
        BASE_SLICE_NS, NICE_MIN,
        entity::{MAX_SLICE_NS, MIN_SLICE_NS},
    };

    /// The number of ticks the simulation counts a run of one of its tasks for.
    ///
    /// A tick is a millisecond, and the time slice is in nanoseconds, so a
    /// tick is a millionth of the accounting unit the tests work in. The tests
    /// are scaled accordingly: one tick is one unit of real time, and a task
    /// charged a unit has had a millionth of a slice.
    const TICK: u64 = 1;

    /// Builds the scheduling state of a task that does nothing when it runs.
    fn entity(nice: i8) -> Arc<Entity> {
        entity_with_slice(nice, BASE_SLICE_NS)
    }

    /// Builds the scheduling state of a task that wants a slice of `slice_ns`.
    fn entity_with_slice(nice: i8, slice_ns: u64) -> Arc<Entity> {
        Entity::with_options(nice, slice_ns)
    }

    /// Wraps `entity` in a fresh entry.
    ///
    /// The task is a real one, so that the runqueue is exercised end to end,
    /// but the tests never run it: the runqueue is driven by hand.
    fn entry(entity: &Arc<Entity>) -> Entry {
        let task = Arc::new(
            TaskOptions::new(|| {})
                .build()
                .expect("a kernel stack for the test task"),
        );
        Entry::new(task, Arc::clone(entity), 0)
    }

    /// Creates a runqueue holding `entities`, all of them placed as newcomers.
    fn runqueue(entities: &[Arc<Entity>]) -> RunQueue {
        let mut rq = RunQueue::new();
        for entity in entities {
            rq.place(entry(entity), true);
        }
        rq
    }

    /// Creates a runqueue holding `count` tasks of the same weight and slice.
    fn runqueue_of(count: usize) -> (RunQueue, Vec<Arc<Entity>>) {
        let entities: Vec<_> = (0..count).map(|_| entity(0)).collect();
        (runqueue(&entities), entities)
    }

    /// Dispatches whatever should run now, and returns the task now running.
    fn dispatch(rq: &mut RunQueue) -> Arc<Entity> {
        let next = rq.pick_next(false).expect("something to run");
        let entity = Arc::clone(&next.entity);
        rq.install(next, true);
        entity
    }

    /// Runs the runqueue for `ticks` ticks, one at a time as a timer would,
    /// charging whoever is running and switching when the algorithm says to.
    ///
    /// Returns the time each entity spent running, indexed as the entities were
    /// given.
    fn tick(rq: &mut RunQueue, entities: &[Arc<Entity>], ticks: u64) -> Vec<u64> {
        let mut ran = alloc::vec![0; entities.len()];
        for _ in 0..ticks {
            // The tick belongs to whoever was running during it, so it is
            // counted before the charge and before any switch.
            if let Some(current) = rq.current_entity() {
                ran[index_of(entities, current)] += TICK;
            }
            rq.charge_delta(TICK);
            if let Some(next) = rq.pick_next(false) {
                rq.install(next, true);
            }
        }
        ran
    }

    /// Returns the index of `entity` in `entities`.
    fn index_of(entities: &[Arc<Entity>], entity: &Arc<Entity>) -> usize {
        entities
            .iter()
            .position(|candidate| Arc::ptr_eq(candidate, entity))
            .expect("a task of the runqueue")
    }

    /// Runs the runqueue until `entity` is the task on the CPU.
    fn run_until(rq: &mut RunQueue, entity: &Arc<Entity>) {
        for _ in 0..10_000 {
            if rq
                .current_entity()
                .is_some_and(|current| Arc::ptr_eq(current, entity))
            {
                return;
            }
            rq.charge_delta(TICK);
            if let Some(next) = rq.pick_next(false) {
                rq.install(next, true);
            }
        }
        panic!("the task never ran");
    }

    /// Runs the runqueue for `ticks` ticks and reports how long each task ran,
    /// in units of a thousandth of a slice.
    fn shares(entities: &[Arc<Entity>], ticks: u64) -> Vec<u64> {
        let mut rq = runqueue(entities);
        dispatch(&mut rq);
        let per_slice = tick(&mut rq, entities, ticks);
        let total: u64 = per_slice.iter().sum();
        per_slice
            .into_iter()
            .map(|time| time * 1000 * TICK / total.max(1))
            .collect()
    }

    /// Returns the lag of `entity`: how much less than its share of the CPU it
    /// has been given, negative if it has had more.
    fn lag(rq: &RunQueue, entity: &Entity) -> i64 {
        rq.avg_vruntime() as i64 - entity.vruntime() as i64
    }

    /// Returns the weighted mean of the virtual runtimes on `rq`, computed from
    /// the tasks that are on it rather than from the runqueue's running sums.
    fn mean_vruntime(rq: &RunQueue) -> u64 {
        let entities: Vec<&Entity> = rq
            .current_entity()
            .into_iter()
            .map(Arc::as_ref)
            .chain(rq.queued_tasks().map(|(_, entity)| entity.as_ref()))
            .filter(|entity| entity.weight() > 0)
            .collect();
        let weight: u64 = entities.iter().map(|entity| entity.weight() as u64).sum();
        if weight == 0 {
            return rq.min_vruntime();
        }
        let weighted: u128 = entities
            .iter()
            .map(|entity| entity.vruntime() as u128 * entity.weight() as u128)
            .sum();
        (weighted / weight as u128) as u64
    }

    #[ktest]
    fn an_empty_runqueue_has_nothing_to_pick() {
        let rq = RunQueue::new();
        assert!(rq.pick_next(false).is_none(), "nothing can be picked");
        assert!(!rq.should_switch(), "nothing can preempt");
        assert!(!rq.can_idle(), "a runqueue that never ran has nothing to wait for");
        assert_eq!(rq.nr_running(), 0);
        assert_eq!(rq.avg_vruntime(), 0);
    }

    #[ktest]
    fn a_newcomer_is_owed_nothing() {
        let (rq, entities) = runqueue_of(3);
        for entity in &entities {
            assert_eq!(entity.vruntime(), 0);
            assert_eq!(lag(&rq, entity), 0, "a newcomer has no lag");
        }
    }

    #[ktest]
    fn the_lags_of_a_runqueue_cancel_out() {
        // Three tasks of equal weight contending, which is the example from the
        // EEVDF write-ups.
        let (mut rq, entities) = runqueue_of(3);
        let (a, b, c) = (&entities[0], &entities[1], &entities[2]);

        let running = dispatch(&mut rq);
        assert!(entities.iter().any(|entity| Arc::ptr_eq(entity, &running)));

        // Over a run of the scheduler, no task is left owing more than it can be
        // given in a turn, and the lags still cancel out.
        for round in 0..500 {
            rq.charge_delta(TICK);
            if let Some(next) = rq.pick_next(false) {
                rq.install(next, true);
            }
            if round % 100 == 99 {
                let lags = [lag(&rq, a), lag(&rq, b), lag(&rq, c)];
                assert!(
                    lags.iter().all(|lag| lag.abs() <= 2 * a.request() as i64),
                    "a lag may be at most a couple of requests, got {lags:?}"
                );
                let sum: i64 = lags.iter().sum();
                assert!(
                    sum.abs() <= 3 * (rq.avg_vruntime() / rq.nr_running() as u64) as i64 + 3,
                    "the lags of a runqueue must cancel, got {lags:?}"
                );
            }
        }
    }

    #[ktest]
    fn a_task_that_has_used_its_share_gives_the_cpu_away() {
        let (mut rq, entities) = runqueue_of(3);
        let running = dispatch(&mut rq);
        // Nothing has been charged yet, so the running task has had none of the
        // CPU that is owed to the others and keeps it.
        assert!(
            rq.pick_next(false).is_none(),
            "a task that has been charged nothing is still eligible"
        );
        // One tick later the other two are behind and it has had more than its
        // share of the one tick, so it must give way.
        rq.charge_delta(TICK);
        assert!(lag(&rq, &running) < 0, "the running task is over its share");
        assert!(
            rq.pick_next(false).is_some(),
            "a task that has had its share does not keep the CPU"
        );
        let next = rq.pick_next(false).expect("a task that is behind");
        assert!(!Arc::ptr_eq(&next.entity, &running));
        assert!(entities.iter().any(|entity| Arc::ptr_eq(entity, &next.entity)));
    }

    #[ktest]
    fn the_only_task_keeps_running() {
        let (mut rq, _) = runqueue_of(1);
        dispatch(&mut rq);
        for _ in 0..100 {
            assert!(rq.pick_next(false).is_none(), "there is nothing to switch to");
            assert!(rq.pick_next(true).is_none(), "not even when yielding");
            rq.charge_delta(TICK);
        }
    }

    #[ktest]
    fn a_task_keeps_running_until_its_lag_is_spent() {
        let (mut rq, entities) = runqueue_of(2);
        let running = dispatch(&mut rq);
        let index = index_of(&entities, &running);
        let other = &entities[1 - index];

        // The running task is charged, and the turn only ends when the charge
        // takes it past the runqueue's virtual time, which is exactly when it
        // stops being eligible.
        let mut turn = 0;
        loop {
            turn += 1;
            assert!(
                rq.pick_next(false).is_none() || !is_eligible(&running, rq.avg_vruntime()),
                "a task that is still eligible keeps the CPU"
            );
            rq.charge_delta(TICK);
            if !is_eligible(&running, rq.avg_vruntime()) {
                break;
            }
            assert!(turn < 10_000, "the turn must end");
        }
        assert!(turn > 0);

        let next = rq.pick_next(false).expect("the task that is behind");
        assert!(!Arc::ptr_eq(&next.entity, &running), "the turn passes on");
        assert!(Arc::ptr_eq(&next.entity, other));
    }

    #[ktest]
    fn a_yield_gives_the_cpu_away() {
        let (mut rq, entities) = runqueue_of(2);
        let first = dispatch(&mut rq);
        assert!(
            rq.pick_next(false).is_none(),
            "without a yield the earliest deadline stays put"
        );
        let next = rq.pick_next(true).expect("a yield moves the other task ahead");
        assert!(!Arc::ptr_eq(&next.entity, &first), "the yielder gives way");
        assert!(Arc::ptr_eq(&next.entity, &entities[1]));
    }

    #[ktest]
    fn a_short_slice_earns_an_earlier_deadline() {
        // Two tasks of equal weight and equal virtual runtime, one of which
        // wants a hundredth of the time of the other.
        let eager = entity_with_slice(0, MIN_SLICE_NS);
        let patient = entity_with_slice(0, MAX_SLICE_NS);
        assert!(eager.deadline() < patient.deadline());

        let mut rq = runqueue(&[Arc::clone(&eager), Arc::clone(&patient)]);
        let next = rq.pick_next(false).expect("something to run");
        assert!(
            Arc::ptr_eq(&next.entity, &eager),
            "the task that asked for the least latency runs first"
        );
        rq.install(next, true);
        // And it does not stop at the front of the queue: it is a request of a
        // hundredth of the other's, so it is ineligible again long before the
        // other one has run.
        rq.charge_delta(TICK);
        let next = rq.pick_next(false).expect("something to run");
        assert!(
            !Arc::ptr_eq(&next.entity, &eager),
            "a short slice buys a turn, not the CPU"
        );
    }

    #[ktest]
    fn a_woken_task_comes_back_to_the_place_it_left() {
        let (mut rq, entities) = runqueue_of(2);
        let sleeper = Arc::clone(&entities[0]);
        dispatch(&mut rq);
        // The sleeper runs and is charged more than its share, so that it goes
        // to sleep owing nothing and with a lag that is not the neutral one.
        run_until(&mut rq, &sleeper);
        rq.charge_delta(TICK);
        rq.set_last_flags(UpdateFlags::Wait);
        let left_with = lag(&rq, &sleeper);
        assert!(left_with < 0, "the sleeper leaves having had too much");
        let dequeued = rq.dequeue_current().expect("the sleeper goes to sleep");
        assert!(Arc::ptr_eq(&dequeued.entity, &sleeper), "it was the sleeper");
        assert_eq!(sleeper.lag(), left_with, "the lag is kept");

        // The other task has the runqueue to itself for a long time, so its
        // virtual time runs on without the sleeper.
        let other = dispatch(&mut rq);
        let others = [Arc::clone(&other)];
        for _ in 0..5000 {
            rq.charge_delta(TICK);
            if let Some(next) = rq.pick_next(false) {
                rq.install(next, true);
            }
        }
        // The virtual time has run a long way past the sleeper while it was
        // gone, which is the whole reason the lag has to be kept: without it the
        // sleeper would come back owed all of this.
        assert!(
            others[0].vruntime() > sleeper.vruntime() + 2 * sleeper.request(),
            "the runqueue's virtual time moved on: {} against {}",
            others[0].vruntime(),
            sleeper.vruntime()
        );

        rq.place(entry(&sleeper), false);
        let owed = lag(&rq, &sleeper);
        assert_eq!(
            owed,
            sleeper.lag() as i64,
            "a woken task keeps the lag it went to sleep with"
        );
        assert!(
            owed.abs() <= 2 * sleeper.request() as i64,
            "and a lag is at most a couple of requests, got {owed}"
        );
        // A task that went to sleep having had too much of the CPU comes back
        // still in that position, so it waits to earn the difference back
        // rather than starting with a burst.
        assert!(!is_eligible(&sleeper, rq.avg_vruntime()));
    }

    #[ktest]
    fn a_woken_task_comes_back_owed_what_it_was_owed() {
        // The other way round: a task that goes to sleep while it is behind
        // keeps the credit, so that a task which blocks in a loop is not starved
        // by the tasks that are ahead of it.
        let (mut rq, entities) = runqueue_of(2);
        let first = Arc::clone(&entities[0]);
        dispatch(&mut rq);
        assert!(Arc::ptr_eq(&first, &rq.current_entity().expect("running").clone()));

        // Let the task that is running be given the CPU, which leaves the other
        // one behind. It is then picked, because it is eligible, and goes to
        // sleep at once without ever running: it is owed everything the first
        // task took.
        for _ in 0..2 {
            rq.charge_delta(TICK);
        }
        let next = rq.pick_next(false).expect("the task behind is picked");
        rq.install(next, true);
        let behind = Arc::clone(&entities[1 - index_of(&entities, &first)]);
        assert!(Arc::ptr_eq(&behind, &rq.current_entity().expect("running").clone()));
        assert!(lag(&rq, &behind) > 0, "it is behind before it runs");

        // The lag has to be read while the task is still on the runqueue,
        // because that is the only time the runqueue's virtual time means
        // anything for a task of it.
        let left_with = lag(&rq, &behind);
        assert!(left_with > 0, "it goes to sleep owed something");
        rq.set_last_flags(UpdateFlags::Wait);
        let dequeued = rq.dequeue_current().expect("it goes to sleep at once");
        assert!(Arc::ptr_eq(&dequeued.entity, &behind));
        assert_eq!(behind.lag(), left_with, "the credit is kept");

        // The task that stayed has the CPU to itself for a long time, so the
        // virtual time runs far past the one that left: without the credit it
        // left with, a task that keeps blocking would be behind for ever.
        for _ in 0..5000 {
            rq.charge_delta(TICK);
            if let Some(next) = rq.pick_next(false) {
                rq.install(next, true);
            }
        }
        assert!(
            first.vruntime() > behind.vruntime() + 2 * behind.request(),
            "the runqueue's virtual time moved on: {} against {}",
            first.vruntime(),
            behind.vruntime()
        );

        rq.place(entry(&behind), false);
        assert_eq!(
            lag(&rq, &behind),
            left_with,
            "the credit is the one the task left with"
        );
        assert!(
            is_eligible(&behind, rq.avg_vruntime()),
            "and it is owed CPU, so it is eligible to run"
        );
    }

    #[ktest]
    fn a_newcomer_does_not_jump_the_queue() {
        let (mut rq, _) = runqueue_of(2);
        dispatch(&mut rq);
        for _ in 0..50 {
            rq.charge_delta(TICK);
            if let Some(next) = rq.pick_next(false) {
                rq.install(next, true);
            }
        }
        let newcomer = entity(0);
        rq.place(entry(&newcomer), true);
        // The newcomer is eligible, so it is not ignored, but the task that has
        // been waiting is further behind and goes first.
        let next = rq.best().expect("something to run");
        assert!(
            !Arc::ptr_eq(&next.entity, &newcomer),
            "a newcomer waits its turn"
        );
        assert!(
            !rq.should_preempt_entity(&newcomer),
            "and does not interrupt the task that is running"
        );
    }

    #[ktest]
    fn equal_tasks_get_equal_cpu() {
        let entities: Vec<_> = (0..4).map(|_| entity(0)).collect();
        let shares = shares(&entities, 40_000);
        for (index, share) in shares.iter().enumerate() {
            assert_eq!(
                *share,
                250,
                "task {index} got {share} per mille of the CPU, expected 250: {shares:?}"
            );
        }
    }

    #[ktest]
    fn a_heavy_task_gets_a_share_proportional_to_its_weight() {
        // A task of nice 0 against a task of nice -20, which weighs 86.7 times
        // as much.
        let light = entity(0);
        let heavy = entity(NICE_MIN);
        let entities = [Arc::clone(&light), Arc::clone(&heavy)];
        let shares = shares(&entities, 200_000);
        let total_weight = light.weight() as u64 + heavy.weight() as u64;
        let expected = heavy.weight() as u64 * 1000 / total_weight;
        // The share of the heavier task is the larger of the two.
        let heavier = if shares[0] > shares[1] { 0 } else { 1 };
        let (got, want) = (shares[heavier], expected);
        assert!(
            got.abs_diff(want) <= 1,
            "expected {want} per mille, got {got}: {shares:?}"
        );
    }

    #[ktest]
    fn a_share_of_three_tasks_matches_their_weights() {
        let entities: Vec<_> = [0, 1, 5].into_iter().map(entity).collect();
        let shares = shares(&entities, 200_000);
        let total_weight: u64 = entities.iter().map(|entity| entity.weight() as u64).sum();
        for (index, entity) in entities.iter().enumerate() {
            let expected = entity.weight() as u64 * 1000 / total_weight;
            assert!(
                shares[index].abs_diff(expected) <= 1,
                "task {index} has weight {} and should get {expected} per mille, got {}: {shares:?}",
                entity.weight(),
                shares[index],
            );
        }
    }

    #[ktest]
    fn the_running_sums_stay_equal_to_the_tasks() {
        // The virtual time is kept as running sums rather than recomputed, so
        // this checks that they stay equal to the mean of the tasks that are
        // actually on the runqueue, over a run long enough for `min_vruntime`
        // to have moved many times over.
        let entities: Vec<_> = [NICE_MIN, 0, 3, 5]
            .into_iter()
            .map(entity)
            .collect();
        let mut rq = runqueue(&entities);
        dispatch(&mut rq);

        for round in 0..3000u64 {
            rq.charge_delta(1 + round % 11);
            if let Some(next) = rq.pick_next(false) {
                rq.install(next, true);
            }
            if round % 17 == 16 {
                assert_eq!(
                    rq.avg_vruntime(),
                    mean_vruntime(&rq),
                    "the running sums drifted on round {round}"
                );
            }
        }
        for entity in &entities {
            assert!(
                entity.stats().nr_switches > 0,
                "every task ran: {:?}",
                entity.stats()
            );
        }
    }

    #[ktest]
    fn a_sleeping_task_leaves_the_runqueue() {
        let (mut rq, _) = runqueue_of(2);
        dispatch(&mut rq);
        rq.set_last_flags(UpdateFlags::Wait);
        assert!(rq.dequeue_current().is_some(), "the task is removed");
        assert_eq!(rq.nr_queued(), 1);
        assert_eq!(rq.nr_running(), 1, "the other task is still runnable");
        assert_eq!(rq.avg_vruntime(), mean_vruntime(&rq), "the sums follow the task");
    }

    #[ktest]
    fn an_exiting_task_is_taken_off_the_runqueue() {
        let (mut rq, _) = runqueue_of(2);
        dispatch(&mut rq);
        rq.set_last_flags(UpdateFlags::Exit);
        assert!(rq.dequeue_current().is_some());
        assert_eq!(rq.nr_running(), 1);
        assert_eq!(rq.avg_vruntime(), mean_vruntime(&rq), "the sums follow the task");
    }

    #[ktest]
    fn a_runqueue_that_ran_can_idle() {
        let (mut rq, _) = runqueue_of(1);
        dispatch(&mut rq);
        // Starting to idle has to be possible once there has been work, or a
        // task that blocks with nothing else to run would spin.
        assert!(rq.can_idle());
        rq.set_last_flags(UpdateFlags::Wait);
        assert!(rq.dequeue_current().is_some(), "the only task blocks");
        assert!(rq.start_idling(), "the CPU starts idling");
        assert!(rq.is_idling());
        assert_eq!(rq.nr_running(), 0, "an idle CPU has nothing runnable");
        assert!(rq.dequeue_current().is_none(), "idle is not removable");
        assert!(rq.start_idling(), "and it keeps idling");
        assert!(!rq.should_switch(), "nothing preempts the idle task");
    }

    #[ktest]
    fn a_runqueue_that_never_ran_does_not_idle() {
        // This is the bootstrap context, which has to be able to finish so that
        // the machine can power off.
        let mut rq = RunQueue::new();
        assert!(!rq.can_idle());
        assert!(!rq.start_idling(), "there is nothing to wait for");
        assert!(!rq.should_switch(), "and nothing to preempt for");
    }

    #[ktest]
    fn a_wakeup_asks_for_a_preemption_only_when_it_deserves_one() {
        let (mut rq, entities) = runqueue_of(2);
        let running = dispatch(&mut rq);
        assert!(!rq.should_preempt_entity(&running), "never from itself");
        // Let the running task run on without giving the CPU away, which is what
        // the scheduler is being asked about: a task that has been waiting.
        for _ in 0..5 {
            rq.charge_delta(TICK);
        }
        let waiter = Arc::clone(&entities[1 - index_of(&entities, &running)]);
        assert!(
            rq.should_preempt_entity(&waiter),
            "a task that has been waiting preempts one that has been running"
        );

        // A task that is not eligible does not, because fairness comes before
        // urgency.
        let greedy = entity(0);
        greedy.set_vruntime(rq.avg_vruntime() + 1);
        assert!(
            !rq.should_preempt_entity(&greedy),
            "a task that has had its share does not preempt"
        );

        // Neither does a task with a later deadline than the running one, even
        // when it is behind: a task that asked for a long slice has asked to
        // run less often, so it does not get to interrupt one that did not.
        let patient = entity_with_slice(0, MAX_SLICE_NS);
        patient.set_vruntime(waiter.vruntime());
        assert!(
            !rq.should_preempt_entity(&patient),
            "a task that asked for a long slice is in no hurry"
        );
    }

    /// A cheap deterministic generator, so that a test can lay out a runqueue
    /// that is not too regular to be worth testing and is the same every run.
    struct Rng(u64);

    impl Rng {
        /// Returns a number in `0..bound`.
        fn next(&mut self, bound: u64) -> u64 {
            // A splitmix64 step, which is nothing special but is not obviously
            // periodic.
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) % bound
        }
    }

    /// Returns the task the algorithm should pick, worked out the slow and
    /// obvious way: every task is looked at, every eligible one is compared by
    /// deadline, and the running task competes with the rest.
    ///
    /// This is what [`RunQueue::pick_next`] is checked against, so it is
    /// deliberately not clever and shares no code with the implementation.
    fn pick_naively(rq: &RunQueue, entities: &[Arc<Entity>], yielding: bool) -> Option<usize> {
        let avg = rq.avg_vruntime();
        let running = rq.current_entity();
        let current_key = running.map(|entity| {
            let request = entity.request();
            let deadline = entity.deadline();
            (
                deadline + if yielding { request } else { 0 },
                entity.vruntime(),
                entity.seq(),
            )
        });

        // The eligible task with the earliest deadline. If the runqueue has
        // come to a state where nothing is eligible, which the weighted mean
        // having been rounded down can bring about, the runqueue still has to run
        // something, and the task that has had least of its share is the one with
        // the earliest deadline.
        let mut best: Option<((u64, u64, u32), usize)> = None;
        let mut eligible = false;
        for (_, entity) in rq.queued_tasks() {
            let key = (entity.deadline(), entity.vruntime(), entity.seq());
            if best.is_none_or(|(best_key, _)| key < best_key) {
                best = Some((key, index_of(entities, entity)));
            }
            if entity.vruntime() <= avg && !eligible {
                eligible = true;
                // The first eligible task in deadline order is the best of them,
                // so nothing later can beat it.
                best = Some((key, index_of(entities, entity)));
                break;
            }
        }
        // The running task only has to be beaten by a task that is eligible, and
        // only if it is eligible itself: an ineligible task is given way to
        // whatever is waiting.
        let (Some(running), Some(current_key)) = (running, current_key) else {
            return best.map(|(_, index)| index);
        };
        if rq.is_idling() {
            return best.map(|(_, index)| index);
        }
        if running.vruntime() > avg {
            return best.map(|(_, index)| index);
        }
        match best {
            Some((key, _)) if key < current_key => best.map(|(_, index)| index),
            _ => None,
        }
    }

    /// Returns the index in `entities` of the task that should be running.
    fn pick_index(rq: &RunQueue, entities: &[Arc<Entity>], yielding: bool) -> Option<usize> {
        let next = rq.pick_next(yielding)?;
        Some(index_of(entities, &next.entity))
    }

    #[ktest]
    fn the_choice_is_the_one_a_slow_implementation_would_make() {
        // A runqueue of tasks with weights, slices and running times that do not
        // follow a pattern, checked against the definition of the rule on every
        // decision for a long run. Nothing here is a coincidence: the generator
        // is fixed, so a failure is a failure of the rule and not of the draw.
        let mut rng = Rng(0x5EED_5EED_1234_ABCD);
        let entities: Vec<_> = (0..7)
            .map(|_| {
                let nice = (rng.next(40) as i8) - 20;
                let slice = MIN_SLICE_NS + rng.next(9 * MIN_SLICE_NS);
                entity_with_slice(nice, slice)
            })
            .collect();
        let mut rq = runqueue(&entities);
        dispatch(&mut rq);

        for round in 0..5000 {
            rq.charge_delta(1 + rng.next(7));
            let expected = pick_naively(&rq, &entities, false);
            let picked = pick_index(&rq, &entities, false);
            if let Some(next) = rq.pick_next(false) {
                rq.install(next, true);
            }
            assert_eq!(
                picked, expected,
                "round {round}: the runqueue picked the wrong task"
            );
        }
    }

    #[ktest]
    fn a_yield_is_the_same_choice_with_the_yielder_late() {
        let mut rng = Rng(0xDEAD_BEEF_0BAD_F00D);
        let entities: Vec<_> = (0..5)
            .map(|_| entity_with_slice((rng.next(40) as i8) - 20, BASE_SLICE_NS))
            .collect();
        let mut rq = runqueue(&entities);
        dispatch(&mut rq);
        for round in 0..2000 {
            rq.charge_delta(1 + rng.next(5));
            let expected = pick_naively(&rq, &entities, true);
            let picked = pick_index(&rq, &entities, true);
            if let Some(next) = rq.pick_next(true) {
                rq.install(next, true);
            }
            assert_eq!(
                picked, expected,
                "round {round}: a yield chose the wrong task"
            );
        }
    }

    /// Returns the largest virtual time a task of `weight` can be charged for
    /// `ticks` ticks of the clock, which is what the clock's resolution is
    /// worth to it.
    fn tick_in_virtual_time(weight: u32, ticks: u64) -> i128 {
        (ticks as i128 * super::super::NSEC_PER_TICK as i128 * 1024) / weight as i128
    }

    /// Runs a runqueue of `entities` for a while, checking after every so often
    /// that each task's lag is within `bound` of zero and that the weighted lags
    /// cancel out.
    ///
    /// This is the property EEVDF is built around: the difference between what a
    /// task has been given and what it was entitled to stays small, and the lags
    /// of a runqueue always cancel. A scheduler that got the eligibility or the
    /// accounting wrong drifts away from both.
    ///
    /// `ticks_of_slack` is how far a lag may reach past a task's own request, in
    /// multiples of the virtual time a tick is worth to that task. The clock is
    /// the floor on what this scheduler can resolve: a decision is only taken on
    /// a tick, so a task is switched out up to a tick after it has had more than
    /// its share, and the mean it is measured against keeps moving for a tick or
    /// two afterwards, which is where the few ticks come from. It is also why a
    /// task that asks for a slice shorter than a tick cannot have it honoured:
    /// the tick is bigger than the request.
    fn check_lags_stay_bounded(entities: &[Arc<Entity>], ticks_of_slack: u64) {
        let mut rq = runqueue(entities);
        dispatch(&mut rq);
        for round in 0..5000 {
            rq.charge_delta(1);
            if let Some(next) = rq.pick_next(false) {
                rq.install(next, true);
            }
            if round % 250 == 249 {
                let avg = rq.avg_vruntime() as i128;
                let mut weighted = 0;
                for entity in entities {
                    let lag = avg - entity.vruntime() as i128;
                    let request = entity.request() as i128;
                    let slack = 2 * request
                        + tick_in_virtual_time(entity.weight(), ticks_of_slack)
                        + request / 8;
                    assert!(
                        lag.abs() <= slack,
                        "round {round}: the lag of a task is {lag}, more than its \
                         request {request} and {ticks_of_slack} ticks of the clock"
                    );
                    weighted += lag * entity.weight() as i128;
                }
                // The mean is a mean, so the weighted lags of the tasks the
                // runqueue is keeping in its sums must cancel to within the
                // rounding of the division.
                let total_weight: i128 = entities.iter().map(|e| e.weight() as i128).sum();
                let accounted: i64 = rq
                    .queued_tasks()
                    .map(|(_, entity)| entity.weight() as i64)
                    .sum();
                let _ = accounted;
                assert!(
                    weighted.abs() <= total_weight * 2,
                    "round {round}: the weighted lags do not cancel: {weighted}"
                );
            }
        }
    }

    #[ktest]
    fn a_lag_never_grows_without_bound() {
        // Slices long enough that the clock is finer than a request, which is
        // the case for a kernel whose slice is worth several ticks. The bound is
        // then within about a tick and a half of the task's own request.
        let mut rng = Rng(0x0123_4567_89AB_CDEF);
        let entities: Vec<_> = (0..6)
            .map(|_| {
                let nice = (rng.next(40) as i8) - 20;
                let slice = 8 * MIN_SLICE_NS + rng.next(20 * MIN_SLICE_NS);
                entity_with_slice(nice, slice)
            })
            .collect();
        check_lags_stay_bounded(&entities, 2);
    }

    #[ktest]
    fn a_lag_stays_bounded_even_for_a_slice_shorter_than_a_tick() {
        // A task that asks for a hundred microseconds of a clock that ticks
        // every millisecond cannot have that honoured: the scheduler can only
        // give it a tick at a time, so its lag swings by a tick. The bound here
        // is the clock's, and the point of the test is that the lag is bounded by
        // it rather than drifting.
        let mut rng = Rng(0x9999_8888_7777_6666);
        let entities: Vec<_> = (0..6)
            .map(|_| {
                let nice = (rng.next(40) as i8) - 20;
                entity_with_slice(nice, MIN_SLICE_NS)
            })
            .collect();
        check_lags_stay_bounded(&entities, 3);
    }

    #[ktest]
    fn a_woken_task_is_placed_so_that_it_can_be_picked() {
        // The whole path a wake takes: the task goes to sleep with a lag, the
        // rest of the system runs on, and it comes back to a place from which
        // the algorithm can choose it, either at once or after the task it is
        // behind has had its turn.
        let mut rng = Rng(0xFEED_FACE_CAFE_BEEF);
        for _ in 0..200 {
            let entities: Vec<_> = (0..4)
                .map(|_| entity_with_slice((rng.next(40) as i8) - 20, BASE_SLICE_NS))
                .collect();
            let mut rq = runqueue(&entities);
            dispatch(&mut rq);

            // Let one task have a turn and then go to sleep.
            let sleeper = Arc::clone(&entities[0]);
            run_until(&mut rq, &sleeper);
            rq.set_last_flags(UpdateFlags::Wait);
            let dequeued = rq.dequeue_current().expect("the task sleeps");
            assert!(Arc::ptr_eq(&dequeued.entity, &sleeper));

            // The rest of the system runs for a while.
            for _ in 0..rng.next(200) {
                rq.charge_delta(1);
                if let Some(next) = rq.pick_next(false) {
                    rq.install(next, true);
                }
            }

            // And it comes back, with the same lag it left with, and the
            // runqueue can still choose between everything on it.
            rq.place(entry(&sleeper), false);
            assert_eq!(
                pick_index(&rq, &entities, false),
                pick_naively(&rq, &entities, false)
            );
        }
    }

    #[ktest]
    fn a_request_is_measured_in_the_tasks_own_virtual_time() {
        // Every task is given the same slice of real time, so the request that
        // slice is worth in virtual time is what the weight scales: a heavy
        // task's slice carries it further in its own virtual time, so it has the
        // smaller request and the earlier deadline of the two.
        let light = entity(0);
        let heavy = entity(NICE_MIN);
        assert!(heavy.weight() > light.weight());
        assert!(heavy.request() < light.request());
        assert!(heavy.deadline() < light.deadline());
        assert_eq!(heavy.slice_ns(), light.slice_ns(), "the same slice of time");
    }

    #[ktest]
    fn a_tick_of_charge_moves_a_heavy_task_too() {
        // The virtual runtime is in nanoseconds precisely so that this is not a
        // problem: a tick of a task of the largest weight has to be visible.
        let heavy = entity(NICE_MIN);
        let before = heavy.vruntime();
        heavy.charge(super::super::NSEC_PER_TICK);
        assert!(
            heavy.vruntime() > before,
            "a heavy task must not be charged nothing for a tick"
        );
    }
}
