// SPDX-License-Identifier: GPL-2.0

//! A self test of the scheduler as a whole, for when it is being brought up.
//!
//! The unit tests beside the algorithm drive a runqueue by hand, which is what
//! makes them able to check fairness exactly. They cannot check the part that
//! matters just as much: the way OSTD drives the scheduler, from creating a task
//! through switching, blocking, waking and exiting to the CPU idling at the end
//! with nothing left to run. That part can only be exercised by a real kernel
//! running real tasks, because the unit test kernel of a crate runs its tests
//! under OSTD's own FIFO scheduler and never calls [`init`](super::init).
//!
//! So this is a small program that a kernel runs once, from `kernel_main`, after
//! [`init`](super::init) and before it returns:
//!
//! ```ignore
//! fn kernel_main() {
//!     sched::init();
//!     arch::init();
//!     vm::init();
//!     sched::selftest::run();
//! }
//! ```
//!
//! It spawns a handful of tasks that yield, one of which blocks until another
//! one wakes it, waits for all of them to finish and prints what the scheduler
//! made of it. It returns rather than power off, because whether the machine
//! then powers off is the idle task's decision, and the fact that it does is
//! itself the last thing this checks.
//!
//! The context that calls [`run`] does not get its turn back: it is the
//! bootstrap context, which the scheduler cannot resume, so everything here
//! after the first yield has to be done from a task.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ostd::{
    sync::WaitQueue,
    task::{Task, TaskOptions},
};

use crate::sched::{self, MIN_SLICE_NS, stats::global_snapshot};

/// How many tasks the self test runs.
const TASKS: usize = 4;

/// How many times each of them yields.
const YIELDS: usize = 50;

/// How long the first part of the self test runs for, in ticks, which has to be
/// longer than one load average interval of five seconds for the load average to
/// have folded anything.
const RUN_FOR: u64 = 7 * ostd::timer::TIMER_FREQ;

/// How long each of the two contending tasks of the second part runs for, in
/// ticks. Long enough that the share each gets is a good deal more than the
/// resolution of the clock.
const CONTEND_FOR: u64 = 5 * ostd::timer::TIMER_FREQ;

/// The nice value of one of the two contending tasks. The other is at nice 0, so
/// the one below is [`NICE_MIN`..] times as heavy, and should get that share of
/// the CPU.
const CONTEND_NICE: i8 = -5;

/// Runs the self test, which never returns.
pub fn run() {
    let done = Arc::new([const { AtomicUsize::new(0) }; TASKS]);
    let gate = Arc::new(WaitQueue::new());
    let opened = Arc::new(AtomicUsize::new(0));
    let woken = Arc::new(AtomicUsize::new(0));

    let mut tasks: Vec<Arc<Task>> = Vec::new();
    for index in 0..TASKS {
        let done = Arc::clone(&done);
        let gate = Arc::clone(&gate);
        let opened = Arc::clone(&opened);
        let woken = Arc::clone(&woken);
        let task = sched::spawn_with_slice(
            TaskOptions::new(move || {
                // The first task blocks until the second one opens the gate,
                // which is what exercises a task going to sleep, the CPU idling
                // with nothing else to run, and the wakeup.
                if index == 0 {
                    gate.wait_until(|| (opened.load(Ordering::SeqCst) > 0).then_some(()));
                    woken.store(1, Ordering::SeqCst);
                }
                if index == 1 {
                    for _ in 0..10 {
                        Task::yield_now();
                    }
                    opened.store(1, Ordering::SeqCst);
                    gate.wake_all();
                }
                for _ in 0..YIELDS {
                    Task::yield_now();
                }
                done[index].store(1, Ordering::SeqCst);
                // The last task out checks the result, because the context that
                // spawned them all cannot come back to do it.
                if index == TASKS - 1 {
                    while done.iter().any(|done| done.load(Ordering::SeqCst) == 0) {
                        Task::yield_now();
                    }
                    // Long enough for the load average to have folded an
                    // interval, so that the check below has something to see.
                    while ostd::timer::Jiffies::elapsed().as_u64() < RUN_FOR {
                        Task::yield_now();
                    }
                    let failures = report(woken.load(Ordering::SeqCst) == 1);
                    // The last part: two tasks that never give the CPU up, one
                    // of them much heavier than the other, to see whether the
                    // CPU is actually shared in proportion to their weights.
                    let (light, heavy) = contend();
                    check_shares(light, heavy, failures);
                }
            }),
            // A short slice, so that the deadline of a woken task matters.
            0,
            MIN_SLICE_NS,
        )
        .expect("a task to run the self test in");
        tasks.push(task);
    }
    ostd::info!("the self test spawned {} tasks", tasks.len());
    // Nothing more happens here. The context that called `run` cannot come back
    // once OSTD switches away from it, which it does at the `yield_now` after
    // `kernel_main` returns, and from then on the tasks run the test.
}

/// Checks what the scheduler made of the self test and writes the verdict to
/// the kernel log, one line per check, so that a run of the kernel is something
/// to read rather than something to take on trust.
fn report(was_woken: bool) -> usize {
    let stats = global_snapshot();
    let cpu = sched::stats_of_cpu(ostd::cpu::CpuId::bsp());
    let loadavg = sched::loadavg();
    let me = Task::current().expect("a task");
    let mine = sched::entity_of(&me).expect("our own scheduling state");

    let mut failures = 0;
    let mut check = |name: &str, holds: bool, detail: &str| {
        if !holds {
            failures += 1;
        }
        ostd::info!(
            "self test: {} {name}{}",
            if holds { "ok" } else { "FAILED" },
            if detail.is_empty() {
                String::new()
            } else {
                format!(" ({detail})")
            },
        );
    };

    let runnable = sched::nr_running_on(ostd::cpu::CpuId::bsp());
    check("the report is running as a task", runnable > 0, &runnable.to_string());
    check("a task that slept was woken", was_woken, "");
    check("the cpu switched tasks", cpu.nr_switches > 0, &cpu.nr_switches.to_string());
    // Every tick the scheduler saw has to be accounted for as either running or
    // idle, to within the tick in flight when the counters were read.
    let ticks_ns = stats.nr_ticks * sched::NSEC_PER_TICK;
    check(
        "the cpu accounted for every tick",
        cpu.exec_ns + cpu.idle_ns + sched::NSEC_PER_TICK >= ticks_ns,
        &format!(
            "{} ns of cpu against {} ticks",
            cpu.exec_ns + cpu.idle_ns,
            stats.nr_ticks
        ),
    );
    check(
        "this task ran more than it waited",
        mine.stats().exec_ns > 0,
        &mine.stats().to_string(),
    );
    check(
        "the load average is counting",
        loadavg.one > 0,
        &loadavg.to_string(),
    );

    ostd::info!("a task of the self test: {}", mine.stats());
    sched::stats::dump();
    failures
}

/// Runs two tasks that contend for the CPU, and returns the nanoseconds of CPU
/// each of them was given.
///
/// Neither task ever yields, so the only thing that decides what each of them
/// gets is the clock deciding, on every tick, to take the CPU away from one of
/// them and give it to the other.
fn contend() -> (u64, u64) {
    let light = Arc::new(AtomicU64::new(0));
    let heavy = Arc::new(AtomicU64::new(0));

    let run = |nice: i8, ran: Arc<AtomicU64>| {
        let task = sched::spawn_with_slice(
            TaskOptions::new(move || {
                let until = ostd::timer::Jiffies::elapsed().as_u64() + CONTEND_FOR;
                while ostd::timer::Jiffies::elapsed().as_u64() < until {
                    // A preemption point, which is the only way a task that
                    // does not give the CPU up voluntarily can be taken away
                    // from: the clock asks for a preemption, and the task has to
                    // reach somewhere the switch can happen.
                    ostd::task::halt_cpu();
                }
                let me = Task::current().expect("a task");
                ran.store(
                    sched::entity_of(&me).expect("our own state").stats().exec_ns,
                    Ordering::SeqCst,
                );
            }),
            nice,
            sched::BASE_SLICE_NS,
        )
        .expect("a task to contend with");
        task
    };

    let light_task = run(0, Arc::clone(&light));
    let heavy_task = run(CONTEND_NICE, Arc::clone(&heavy));

    // This task has to be out of the way: it is the one that collects the
    // result, and it is neither of the tasks that is being measured.
    let until = ostd::timer::Jiffies::elapsed().as_u64() + CONTEND_FOR;
    while ostd::timer::Jiffies::elapsed().as_u64() < until + 1 {
        Task::yield_now();
    }
    // And out of the runqueue, so that the last of the CPU goes to the two
    // tasks rather than to this one.
    sched::stats::dump();
    let _ = (light_task, heavy_task);
    (light.load(Ordering::SeqCst), heavy.load(Ordering::SeqCst))
}

/// Checks that two contending tasks were given the CPU in proportion to their
/// weights, which is the property the whole algorithm exists for.
fn check_shares(light: u64, heavy: u64, mut failures: usize) {
    let light_weight = sched::weight_from_nice(0) as u64;
    let heavy_weight = sched::weight_from_nice(CONTEND_NICE) as u64;
    let expected = heavy_weight * 100 / light_weight;
    let got = if light == 0 {
        0
    } else {
        heavy * 100 / light
    };
    // The clock decides when a task may be taken away, so a share is only ever
    // as good as a tick of CPU, which over a few seconds of running time is a
    // few parts in a hundred.
    let tolerance = 5 * expected / 100 + 2;
    let holds = light > 0 && heavy > 0 && got.abs_diff(expected) <= tolerance;
    if !holds {
        failures += 1;
    }
    ostd::info!(
        "self test: {} the cpu was shared in proportion to the weights:          {} ns to a task of weight {light_weight} and {} ns to one of weight          {heavy_weight}, which is {got} against an expected {expected} per cent",
        if holds { "ok" } else { "FAILED" },
        light,
        heavy,
    );

    if failures == 0 {
        ostd::info!("self test: PASSED");
    } else {
        ostd::warn!("self test: {failures} checks FAILED");
    }
}

