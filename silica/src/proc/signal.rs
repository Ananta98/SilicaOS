// SPDX-License-Identifier: GPL-2.0

//! Signal handling (FreeBSD `kern_sig.c`).
//!
//! Manages POSIX signal dispositions (`struct sigacts`), per-thread signal masks
//! (`sigset_t`), pending signal queues, and delivery rules.

/// Standard signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
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
    SIGCHLD = 17,
    SIGCONT = 18,
    SIGSTOP = 19,
    SIGTSTP = 20,
    SIGTTIN = 21,
    SIGTTOU = 22,
    SIGWINCH = 28,
}

impl Signal {
    pub const fn as_u32(&self) -> u32 {
        *self as u32
    }

    pub const fn from_u32(val: u32) -> Option<Self> {
        match val {
            1 => Some(Self::SIGHUP),
            2 => Some(Self::SIGINT),
            3 => Some(Self::SIGQUIT),
            4 => Some(Self::SIGILL),
            5 => Some(Self::SIGTRAP),
            6 => Some(Self::SIGABRT),
            7 => Some(Self::SIGBUS),
            8 => Some(Self::SIGFPE),
            9 => Some(Self::SIGKILL),
            10 => Some(Self::SIGUSR1),
            11 => Some(Self::SIGSEGV),
            12 => Some(Self::SIGUSR2),
            13 => Some(Self::SIGPIPE),
            14 => Some(Self::SIGALRM),
            15 => Some(Self::SIGTERM),
            17 => Some(Self::SIGCHLD),
            18 => Some(Self::SIGCONT),
            19 => Some(Self::SIGSTOP),
            20 => Some(Self::SIGTSTP),
            21 => Some(Self::SIGTTIN),
            22 => Some(Self::SIGTTOU),
            28 => Some(Self::SIGWINCH),
            _ => None,
        }
    }

    /// Whether this signal can be caught or ignored.
    pub const fn is_unblockable(&self) -> bool {
        matches!(self, Self::SIGKILL | Self::SIGSTOP)
    }
}

/// 64-bit signal bitmask (`sigset_t`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigSet(pub u64);

impl SigSet {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn all() -> Self {
        Self(!0)
    }

    pub fn add(&mut self, sig: Signal) {
        self.0 |= 1 << (sig.as_u32() - 1);
    }

    pub fn del(&mut self, sig: Signal) {
        self.0 &= !(1 << (sig.as_u32() - 1));
    }

    pub fn contains(&self, sig: Signal) -> bool {
        (self.0 & (1 << (sig.as_u32() - 1))) != 0
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Removes unblockable signals from the mask (SIGKILL and SIGSTOP).
    pub fn sanitize_mask(&mut self) {
        self.del(Signal::SIGKILL);
        self.del(Signal::SIGSTOP);
    }
}

/// Signal disposition action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SigHandler {
    /// Default signal action (`SIG_DFL`).
    #[default]
    Default,
    /// Ignore the signal (`SIG_IGN`).
    Ignore,
    /// User-space signal handler function pointer.
    Handler(usize),
}

/// Signal action configuration (`struct sigaction`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SigAction {
    pub sa_handler: SigHandler,
    pub sa_mask: SigSet,
    pub sa_flags: u32,
}

/// Process signal actions table (mirrors FreeBSD `struct sigacts`).
pub struct SigActs {
    pub actions: [SigAction; 64],
}

impl SigActs {
    pub fn new() -> Self {
        Self {
            actions: [SigAction::default(); 64],
        }
    }

    pub fn get(&self, sig: Signal) -> SigAction {
        self.actions[(sig.as_u32() - 1) as usize]
    }

    pub fn set(&mut self, sig: Signal, act: SigAction) {
        if !sig.is_unblockable() {
            self.actions[(sig.as_u32() - 1) as usize] = act;
        }
    }
}

impl Default for SigActs {
    fn default() -> Self {
        Self::new()
    }
}

/// Pending signals queue.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SigQueue {
    pub pending: SigSet,
}

impl SigQueue {
    pub const fn new() -> Self {
        Self {
            pending: SigSet::empty(),
        }
    }

    pub fn post(&mut self, sig: Signal) {
        self.pending.add(sig);
    }

    pub fn clear(&mut self, sig: Signal) {
        self.pending.del(sig);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Pops the lowest-numbered pending signal that is not masked.
    pub fn pop_unmasked(&mut self, mask: SigSet) -> Option<Signal> {
        let deliverable = self.pending.0 & !mask.0;
        if deliverable == 0 {
            return None;
        }
        let bit = deliverable.trailing_zeros();
        let sig_no = bit + 1;
        let sig = Signal::from_u32(sig_no)?;
        self.pending.del(sig);
        Some(sig)
    }
}
