// SPDX-License-Identifier: GPL-2.0
#![allow(non_camel_case_types)]


pub type tcflag_t = u32;
pub type cc_t = u8;
pub type speed_t = u32;

pub const NCCS: usize = 32;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct termios {
    pub c_iflag: tcflag_t,
    pub c_oflag: tcflag_t,
    pub c_cflag: tcflag_t,
    pub c_lflag: tcflag_t,
    pub c_line: cc_t,
    pub c_cc: [cc_t; NCCS],
    pub c_ispeed: speed_t,
    pub c_ospeed: speed_t,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct winsize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

impl termios {
    pub const SIZE: usize = 60;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.c_iflag.to_ne_bytes());
        buf[4..8].copy_from_slice(&self.c_oflag.to_ne_bytes());
        buf[8..12].copy_from_slice(&self.c_cflag.to_ne_bytes());
        buf[12..16].copy_from_slice(&self.c_lflag.to_ne_bytes());
        buf[16] = self.c_line;
        buf[17..49].copy_from_slice(&self.c_cc);
        // 49..52 is padding
        buf[52..56].copy_from_slice(&self.c_ispeed.to_ne_bytes());
        buf[56..60].copy_from_slice(&self.c_ospeed.to_ne_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        let mut cc = [0u8; NCCS];
        cc.copy_from_slice(&buf[17..49]);
        Self {
            c_iflag: u32::from_ne_bytes(buf[0..4].try_into().unwrap()),
            c_oflag: u32::from_ne_bytes(buf[4..8].try_into().unwrap()),
            c_cflag: u32::from_ne_bytes(buf[8..12].try_into().unwrap()),
            c_lflag: u32::from_ne_bytes(buf[12..16].try_into().unwrap()),
            c_line: buf[16],
            c_cc: cc,
            c_ispeed: u32::from_ne_bytes(buf[52..56].try_into().unwrap()),
            c_ospeed: u32::from_ne_bytes(buf[56..60].try_into().unwrap()),
        }
    }
}

impl winsize {
    pub const SIZE: usize = 8;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..2].copy_from_slice(&self.ws_row.to_ne_bytes());
        buf[2..4].copy_from_slice(&self.ws_col.to_ne_bytes());
        buf[4..6].copy_from_slice(&self.ws_xpixel.to_ne_bytes());
        buf[6..8].copy_from_slice(&self.ws_ypixel.to_ne_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        Self {
            ws_row: u16::from_ne_bytes(buf[0..2].try_into().unwrap()),
            ws_col: u16::from_ne_bytes(buf[2..4].try_into().unwrap()),
            ws_xpixel: u16::from_ne_bytes(buf[4..6].try_into().unwrap()),
            ws_ypixel: u16::from_ne_bytes(buf[6..8].try_into().unwrap()),
        }
    }
}

// c_lflag bits
pub const ISIG: tcflag_t = 0o000001;
pub const ICANON: tcflag_t = 0o000002;
pub const ECHO: tcflag_t = 0o000010;
pub const ECHOE: tcflag_t = 0o000020;
pub const ECHOK: tcflag_t = 0o000040;
pub const ECHONL: tcflag_t = 0o000100;
pub const NOFLSH: tcflag_t = 0o000200;
pub const TOSTOP: tcflag_t = 0o000400;
pub const IEXTEN: tcflag_t = 0o100000;

// c_cc indices
pub const VINTR: usize = 0;
pub const VQUIT: usize = 1;
pub const VERASE: usize = 2;
pub const VKILL: usize = 3;
pub const VEOF: usize = 4;
pub const VTIME: usize = 5;
pub const VMIN: usize = 6;
pub const VSWTC: usize = 7;
pub const VSTART: usize = 8;
pub const VSTOP: usize = 9;
pub const VSUSP: usize = 10;
pub const VEOL: usize = 11;
pub const VREPRINT: usize = 12;
pub const VDISCARD: usize = 13;
pub const VWERASE: usize = 14;
pub const VLNEXT: usize = 15;
pub const VEOL2: usize = 16;

// c_iflag bits
pub const IGNBRK: tcflag_t = 0o000001;
pub const BRKINT: tcflag_t = 0o000002;
pub const IGNPAR: tcflag_t = 0o000004;
pub const PARMRK: tcflag_t = 0o000010;
pub const INPCK: tcflag_t  = 0o000020;
pub const ISTRIP: tcflag_t = 0o000040;
pub const INLCR: tcflag_t  = 0o000100;
pub const IGNCR: tcflag_t  = 0o000200;
pub const ICRNL: tcflag_t  = 0o000400;
pub const IXON: tcflag_t   = 0o002000;
pub const IXANY: tcflag_t  = 0o004000;
pub const IXOFF: tcflag_t  = 0o010000;

// c_oflag bits
pub const OPOST: tcflag_t  = 0o000001;
pub const ONLCR: tcflag_t  = 0o000004;
pub const OCRNL: tcflag_t  = 0o000010;
pub const ONOCR: tcflag_t  = 0o000020;
pub const ONLRET: tcflag_t = 0o000040;

pub type Termios = termios;

impl Default for termios {
    fn default() -> Self {
        let mut cc = [0; NCCS];
        cc[VEOF] = 4;     // ^D
        cc[VEOL] = 0;
        cc[VERASE] = 127; // DEL
        cc[VINTR] = 3;    // ^C
        cc[VKILL] = 21;   // ^U
        cc[VQUIT] = 28;   // ^\
        cc[VSTART] = 17;  // ^Q
        cc[VSTOP] = 19;   // ^S
        cc[VSUSP] = 26;   // ^Z

        Self {
            c_iflag: ICRNL | IXON,
            c_oflag: OPOST | ONLCR,
            c_cflag: 0,
            c_lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK,
            c_line: 0,
            c_cc: cc,
            c_ispeed: 38400,
            c_ospeed: 38400,
        }
    }
}
