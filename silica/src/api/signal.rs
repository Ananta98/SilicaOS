// SPDX-License-Identifier: GPL-2.0

//! Signal dispositions, masks and payloads (FreeBSD `kern_sig.c`).
//!
//! Everything here is a shape or a constant that user space and the kernel agree
//! on, so the layouts are fixed by the Linux x86-64 ABI and the discriminants by
//! the Linux signal numbers. The *mechanism* -- queueing a signal, choosing a
//! thread, building a frame, running a handler -- is in [`crate::proc::signal`],
//! and the frame layout is in [`crate::arch::x86_64::signal`].
//!
//! [`crate::proc::signal`]: crate::proc::signal

use core::fmt;

/// The lowest signal number.
pub const SIGRTMIN: u32 = 34;
/// The highest signal number reserved for `kill(2)`.
pub const SIGRTMAX: u32 = 64;
/// The highest number this kernel knows, `SIGSYS`.
pub const SIGNAL_MAX: u32 = 31;

/// Signals, with their Linux numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
#[allow(missing_docs)]
pub enum Signal {
    SIGHUP = 1,
    SIGINT = 2,
    SIGQUIT = 3,
    SIGILL = 4,
    SIGTRAP = 5,
    SIGABRT = 6,
    SIGBUS = 7,
    SIGFPE = 8,
    SIGKILL = 9,
    SIGUSR1 = 10,
    SIGSEGV = 11,
    SIGUSR2 = 12,
    SIGPIPE = 13,
    SIGALRM = 14,
    SIGTERM = 15,
    SIGSTKFLT = 16,
    SIGCHLD = 17,
    SIGCONT = 18,
    SIGSTOP = 19,
    SIGTSTP = 20,
    SIGTTIN = 21,
    SIGTTOU = 22,
    SIGURG = 23,
    SIGXCPU = 24,
    SIGXFSZ = 25,
    SIGVTALRM = 26,
    SIGPROF = 27,
    SIGWINCH = 28,
    SIGIO = 29,
    SIGPWR = 30,
    SIGSYS = 31,
}

/// Every signal by number, indexed by that number.
///
/// The single place the numbering is written down. `Display` and `from_u32`
/// both go through the enum rather than keeping a second list, so the only thing
/// that can be wrong is this table, and the compiler checks it.
const TABLE: [Option<Signal>; SIGNAL_MAX as usize + 1] = {
    use Signal::*;
    [
        None,
        Some(SIGHUP),
        Some(SIGINT),
        Some(SIGQUIT),
        Some(SIGILL),
        Some(SIGTRAP),
        Some(SIGABRT),
        Some(SIGBUS),
        Some(SIGFPE),
        Some(SIGKILL),
        Some(SIGUSR1),
        Some(SIGSEGV),
        Some(SIGUSR2),
        Some(SIGPIPE),
        Some(SIGALRM),
        Some(SIGTERM),
        Some(SIGSTKFLT),
        Some(SIGCHLD),
        Some(SIGCONT),
        Some(SIGSTOP),
        Some(SIGTSTP),
        Some(SIGTTIN),
        Some(SIGTTOU),
        Some(SIGURG),
        Some(SIGXCPU),
        Some(SIGXFSZ),
        Some(SIGVTALRM),
        Some(SIGPROF),
        Some(SIGWINCH),
        Some(SIGIO),
        Some(SIGPWR),
        Some(SIGSYS),
    ]
};

impl Signal {
    /// The Linux signal number.
    pub const fn as_u32(&self) -> u32 {
        *self as u32
    }

    /// The signal with this number, if the kernel knows it.
    ///
    /// A table rather than a cast: the discriminants happen to be dense, but
    /// transmuting would be the only `unsafe` in the ABI, and the crate forbids
    /// it. It also cannot drift, because the compiler checks the table against
    /// the enum.
    pub const fn from_u32(val: u32) -> Option<Self> {
        if val == 0 || val > SIGNAL_MAX {
            return None;
        }
        TABLE[val as usize]
    }

    /// Whether this signal may be caught, ignored, or blocked.
    ///
    /// SIGKILL and SIGSTOP cannot: the kernel acts on them whatever a process
    /// asks for, which is what makes them able to stop an unkillable process.
    pub const fn is_unblockable(&self) -> bool {
        matches!(self, Self::SIGKILL | Self::SIGSTOP)
    }

    /// Whether the default action is to stop the process rather than kill it.
    pub const fn default_is_stop(&self) -> bool {
        matches!(self, Self::SIGSTOP | Self::SIGTSTP | Self::SIGTTIN | Self::SIGTTOU)
    }

    /// Whether the default action is to do nothing.
    pub const fn default_is_ignore(&self) -> bool {
        matches!(self, Self::SIGCHLD | Self::SIGCONT | Self::SIGURG | Self::SIGWINCH)
    }

    /// Whether delivering this signal should record a core dump.
    ///
    /// Whether one is actually written also depends on `RLIMIT_CORE`, which
    /// nothing enforces yet.
    pub const fn default_dumps_core(&self) -> bool {
        matches!(self, Self::SIGQUIT | Self::SIGILL | Self::SIGABRT | Self::SIGFPE | Self::SIGSEGV)
    }

    /// The index of this signal's bit in a [`SigSet`].
    ///
    /// Signals are numbered from one, so this is `as_u32() - 1`.
    pub const fn bit(&self) -> usize {
        (self.as_u32() - 1) as usize
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Matched on the enum, not looked up in a second table: the compiler then
        // fails to compile if a signal is added and this is not updated.
        let name = match self {
            Self::SIGHUP => "SIGHUP",
            Self::SIGINT => "SIGINT",
            Self::SIGQUIT => "SIGQUIT",
            Self::SIGILL => "SIGILL",
            Self::SIGTRAP => "SIGTRAP",
            Self::SIGABRT => "SIGABRT",
            Self::SIGBUS => "SIGBUS",
            Self::SIGFPE => "SIGFPE",
            Self::SIGKILL => "SIGKILL",
            Self::SIGUSR1 => "SIGUSR1",
            Self::SIGSEGV => "SIGSEGV",
            Self::SIGUSR2 => "SIGUSR2",
            Self::SIGPIPE => "SIGPIPE",
            Self::SIGALRM => "SIGALRM",
            Self::SIGTERM => "SIGTERM",
            Self::SIGSTKFLT => "SIGSTKFLT",
            Self::SIGCHLD => "SIGCHLD",
            Self::SIGCONT => "SIGCONT",
            Self::SIGSTOP => "SIGSTOP",
            Self::SIGTSTP => "SIGTSTP",
            Self::SIGTTIN => "SIGTTIN",
            Self::SIGTTOU => "SIGTTOU",
            Self::SIGURG => "SIGURG",
            Self::SIGXCPU => "SIGXCPU",
            Self::SIGXFSZ => "SIGXFSZ",
            Self::SIGVTALRM => "SIGVTALRM",
            Self::SIGPROF => "SIGPROF",
            Self::SIGWINCH => "SIGWINCH",
            Self::SIGIO => "SIGIO",
            Self::SIGPWR => "SIGPWR",
            Self::SIGSYS => "SIGSYS",
        };
        f.write_str(name)
    }
}

/// The number of machine words in a [`SigSet`].
///
/// Linux `sigset_t` is `unsigned long __val[16]`, so it is 1024 bits on x86-64
/// even though only signals 1..=31 exist. The width is fixed by the ABI and a
/// narrower kernel-side type would corrupt every `rt_sigprocmask` call.
pub const SIGSET_WORDS: usize = 16;

/// A signal mask (`sigset_t`).
///
/// Signals are numbered from one, so signal `n` is bit `n - 1`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigSet(pub [u64; SIGSET_WORDS]);

impl SigSet {
    pub const fn empty() -> Self {
        Self([0; SIGSET_WORDS])
    }

    pub const fn full() -> Self {
        Self([u64::MAX; SIGSET_WORDS])
    }

    /// The mask a thread starts with: nothing blocked.
    ///
    /// POSIX leaves the initial mask empty, and Linux does too, so a signal sent
    /// before a thread has called `rt_sigprocmask` is delivered rather than
    /// deferred. A kernel that blocked everything here would silently drop every
    /// signal until the first `rt_sigprocmask`, which is a difference a program
    /// can observe and cannot work around.
    pub const fn initial() -> Self {
        Self::empty()
    }

    pub fn add(&mut self, sig: Signal) {
        self.0[sig.bit() / 64] |= 1 << (sig.bit() % 64);
    }

    pub fn remove(&mut self, sig: Signal) {
        self.0[sig.bit() / 64] &= !(1 << (sig.bit() % 64));
    }

    /// Blocks `sig`.
    ///
    /// A signal that cannot be blocked is left alone rather than recorded: if
    /// the mask claimed to block SIGKILL, a later `rt_sigprocmask` would hand
    /// user space a promise the kernel does not keep.
    pub fn block(&mut self, sig: Signal) {
        if !sig.is_unblockable() {
            self.add(sig);
        }
    }

    /// Unblocks `sig`.
    pub fn unblock(&mut self, sig: Signal) {
        self.remove(sig);
    }

    pub fn contains(&self, sig: Signal) -> bool {
        (self.0[sig.bit() / 64] & (1 << (sig.bit() % 64))) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&w| w == 0)
    }

    /// Removes the signals that cannot be blocked or ignored.
    pub fn clear_unblockable(&mut self) {
        self.remove(Signal::SIGKILL);
        self.remove(Signal::SIGSTOP);
    }

    /// The lowest-numbered signal in `self` that is not in `mask`.
    ///
    /// POSIX requires the lowest-numbered deliverable signal to be taken first.
    pub fn lowest_unmasked(&self, mask: &SigSet) -> Option<Signal> {
        (0..SIGNAL_MAX as usize)
            .find(|&bit| {
                let word = bit / 64;
                let one = 1u64 << (bit % 64);
                self.0[word] & one != 0 && mask.0[word] & one == 0
            })
            .and_then(|bit| Signal::from_u32(bit as u32 + 1))
    }
}

/// What a handler should do when the signal arrives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SigHandler {
    /// The signal's default action (`SIG_DFL`).
    #[default]
    Default,
    /// Discard the signal (`SIG_IGN`).
    Ignore,
    /// A user-space handler. Under `SA_SIGINFO` it takes three arguments
    /// (signo, `siginfo_t`, `ucontext_t`); otherwise one (signo).
    Handler(usize),
}

/// Flags from `struct sigaction.sa_flags`.
///
/// These are Linux's values, which are mostly in the high bits -- they are not
/// the small numbers the same names have in other ABIs.
pub mod flags {
    /// Do not report this thread stopping or continuing (`SIGCHLD`).
    pub const SA_NOCLDSTOP: u32 = 0x0000_0001;
    /// Do not turn a stopping child into a waitable zombie (`SIGCHLD`).
    pub const SA_NOCLDWAIT: u32 = 0x0000_0002;
    /// The handler takes `siginfo_t` and `ucontext_t` as well as the number.
    pub const SA_SIGINFO: u32 = 0x0000_0004;
    /// Run the handler on the alternate signal stack (`sigaltstack`).
    pub const SA_ONSTACK: u32 = 0x0800_0000;
    /// Restart an interrupted syscall instead of reporting `EINTR`.
    pub const SA_RESTART: u32 = 0x1000_0000;
    /// Do not block `sig` itself while its handler runs.
    pub const SA_NODEFER: u32 = 0x4000_0000;
    /// Reset the disposition to `SIG_DFL` once the handler is entered.
    pub const SA_RESETHAND: u32 = 0x8000_0000;
}

/// One signal disposition (`struct sigaction`).
///
/// The kernel-side view. The C structure is 152 bytes and carries the handler,
/// a 128-byte mask, the flags, and the restorer address; see
/// [`crate::syscall::proc::signal`] for the marshalling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigAction {
    pub sa_handler: SigHandler,
    /// Blocked while the handler runs, unless `SA_NODEFER`.
    pub sa_mask: SigSet,
    pub sa_flags: u32,
    /// Where the handler returns to.
    ///
    /// `rt_sigaction(2)` refuses a handler without one: there is nowhere else for
    /// control to go, and the trampoline the kernel would supply is not
    /// installed anywhere yet.
    pub sa_restorer: usize,
}

impl SigAction {
    /// Whether this disposition delivers to user space at all.
    pub const fn delivers_to_handler(&self) -> bool {
        matches!(self.sa_handler, SigHandler::Handler(_))
    }

    /// The handler address, if this disposition delivers to one.
    pub fn sa_handler_addr(&self) -> usize {
        match self.sa_handler {
            SigHandler::Handler(addr) => addr,
            SigHandler::Default | SigHandler::Ignore => 0,
        }
    }

    /// The mask in force while the handler runs.
    ///
    /// `sa_mask` plus the signal itself, unless `SA_NODEFER`.
    pub fn effective_mask(&self, sig: Signal) -> SigSet {
        if self.sa_flags & flags::SA_NODEFER != 0 {
            self.sa_mask
        } else {
            let mut mask = self.sa_mask;
            mask.add(sig);
            mask
        }
    }
}

/// The number of signal dispositions, one per signal number.
pub const SIGACTS_LEN: usize = SIGNAL_MAX as usize;

/// The disposition table of a process (`struct sigacts`).
///
/// Shared by every thread in the process: `signal(2)` and `rt_sigaction(2)` are
/// per-process, while the mask a handler runs under is per-thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigActs {
    pub actions: [SigAction; SIGACTS_LEN],
}

impl SigActs {
    pub const fn new() -> Self {
        Self {
            actions: [SigAction {
                sa_handler: SigHandler::Default,
                sa_mask: SigSet::empty(),
                sa_flags: 0,
                sa_restorer: 0,
            }; SIGACTS_LEN],
        }
    }

    pub fn get(&self, sig: Signal) -> SigAction {
        self.actions[sig.bit()]
    }

    /// Installs a disposition, unless the signal cannot be caught.
    pub fn set(&mut self, sig: Signal, act: SigAction) {
        if !sig.is_unblockable() {
            self.actions[sig.bit()] = act;
        }
    }
}

impl Default for SigActs {
    fn default() -> Self {
        Self::new()
    }
}

/// Where `si_code` says a signal came from.
pub mod code {
    /// Sent by `kill(2)`, `raise(2)`, or `tkill(2)`.
    pub const USER: i32 = 0;
    /// Sent by the kernel.
    pub const KERNEL: i32 = 0x80;
    /// An address in user space that is not mapped.
    pub const SEGV_MAPERR: i32 = 1;
    /// An address in user space mapped without the access that was attempted.
    pub const SEGV_ACCERR: i32 = 2;
    /// The process's stack guard page was hit.
    pub const SEGV_BNDERR: i32 = 3;
    /// An unaligned address.
    pub const BUS_ADRALN: i32 = 1;
    /// An address that is not on the page boundary.
    pub const BUS_ADRERR: i32 = 2;
    /// An invalid floating-point operation.
    pub const FPE_INTDIV: i32 = 1;
    /// An instruction operand that is not representable.
    pub const FPE_INTOVF: i32 = 2;
}

/// A signal and why it was sent (`struct siginfo_t`).
///
/// 128 bytes, laid out as user space expects. The union at offset 16 is
/// represented by the widest member plus explicit fields, so nothing is read
/// that the kernel did not write.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SigInfo {
    /// The signal number. This is `si_signo`, at offset 0.
    pub si_signo: i32,
    /// A number the sender chose to pass through, at offset 4.
    pub si_errno: i32,
    /// One of [`code`], at offset 8.
    pub si_code: i32,
    /// Padding to align the union that follows.
    pub _pad0: i32,
    /// The offending address, for `SIGSEGV` and `SIGBUS`.
    pub si_addr: u64,
    /// The sending process, for `SIGCHLD`.
    pub si_pid: i32,
    /// The sending process' effective user, for `SIGCHLD`.
    pub si_uid: u32,
    /// The child's exit status, for `SIGCHLD`.
    pub si_status: i32,
    /// Padding to align the union that follows.
    pub _pad1: i32,
    /// The sender's `utime`, for `SIGCHLD`.
    pub si_utime: u64,
    /// The sender's `stime`, for `SIGCHLD`.
    pub si_stime: u64,
    /// The value the sender passed, for signals that carry one.
    pub si_value: u64,
    /// Padding to fill the union out to 128 bytes.
    pub _pad2: [u64; 8],
}

impl SigInfo {
    /// The size of the C structure, which is what user space allocates.
    pub const LEN: usize = 128;

    /// An event the kernel raises itself.
    pub fn kernel(sig: Signal, code: i32) -> Self {
        Self {
            si_signo: sig.as_u32() as i32,
            si_code: code,
            ..Self::default()
        }
    }

    /// A signal sent by a process.
    pub fn from_user(sig: Signal, sender_pid: u32) -> Self {
        Self {
            si_signo: sig.as_u32() as i32,
            si_code: code::USER,
            si_pid: sender_pid as i32,
            si_uid: 0,
            ..Self::default()
        }
    }

    /// A fault, with the address that faulted.
    pub fn fault(sig: Signal, code: i32, addr: usize) -> Self {
        Self {
            si_signo: sig.as_u32() as i32,
            si_code: code,
            si_addr: addr as u64,
            ..Self::default()
        }
    }
}

/// Flags from `stack_t.ss_flags`.
pub mod ss_flags {
    /// The alternate stack is disabled; `sigaltstack` only disables it.
    pub const DISABLED: u32 = 1 << 0;
    /// The stack runs down, as `sigaltstack` describes a downward-growing one.
    pub const ONSTACK: u32 = 1 << 1;
}

/// An alternate signal stack (`stack_t`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigStack {
    pub ss_sp: usize,
    pub ss_flags: u32,
    /// Padding, because `ss_flags` is a 32-bit field followed by a 64-bit one.
    pub _pad: u32,
    pub ss_size: usize,
}

impl SigStack {
    /// Whether the alternate stack is in use.
    pub const fn enabled(&self) -> bool {
        self.ss_flags & ss_flags::DISABLED == 0 && self.ss_size > 0
    }

    /// Whether the stack pointer is inside this alternate stack.
    ///
    /// Used to avoid building a frame on a stack that cannot take one -- the
    /// case that matters is a fault on the process's own stack, which has no
    /// room left by definition.
    pub const fn contains(&self, sp: usize) -> bool {
        self.enabled() && sp >= self.ss_sp && sp < self.ss_sp + self.ss_size
    }
}