// SPDX-License-Identifier: GPL-2.0

pub const TCGETS: u32 = 0x5401;
pub const TCSETS: u32 = 0x5402;
pub const TCSETSW: u32 = 0x5403;
pub const TCSETSF: u32 = 0x5404;

pub const TCSBRK: u32 = 0x5409;
pub const TCXONC: u32 = 0x540A;
pub const TCFLSH: u32 = 0x540B;

pub const TIOCGWINSZ: u32 = 0x5413;
pub const TIOCSWINSZ: u32 = 0x5414;

pub const TIOCGPGRP: u32 = 0x540F;
pub const TIOCSPGRP: u32 = 0x5410;

pub const TIOCSCTTY: u32 = 0x540E;
pub const TIOCNOTTY: u32 = 0x5422;
pub const FIONREAD: u32 = 0x541B;
