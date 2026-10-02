// SPDX-License-Identifier: GPL-2.0

//! System Console Driver
//!
//! Provides the primary system console (`/dev/console`).
//! Integrates the line discipline and delegates to the VGA text mode driver.

use alloc::string::String;
use crate::drivers::CharacterDevice;
use crate::drivers::char::tty::ldisc::LineDiscipline;
use crate::errno::Result;

/// The main system console device
pub struct Console {
    ldisc: LineDiscipline,
}

impl Console {
    pub fn new() -> Self {
        Self {
            ldisc: LineDiscipline::new(),
        }
    }

    /// Process an incoming character from the keyboard
    pub fn handle_input(&self, ch: u8) {
        // Feed into line discipline
        self.ldisc.receive_char(ch, &mut |echo_char| {
            if let Some(cons_lock) = crate::drivers::drm::get_console() {
                let mut cons = cons_lock.lock();
                cons.write_byte(echo_char);
            }
            ostd::early_print!("{}", echo_char as char);
        });
    }
}

impl CharacterDevice for Console {
    fn name(&self) -> String {
        String::from("console")
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        self.ldisc.read(buf)
    }

    fn write(&self, buf: &[u8]) -> Result<usize> {
        let mut written = 0;
        
        // Output processing according to termios (OPOST, ONLCR)
        let termios = self.ldisc.termios.lock();
        let opost = (termios.c_oflag & crate::api::termios::OPOST) != 0;
        let onlcr = (termios.c_oflag & crate::api::termios::ONLCR) != 0;
        drop(termios);

        for &b in buf {
            if opost && b == b'\n' && onlcr {
                if let Some(cons_lock) = crate::drivers::drm::get_console() {
                    let mut cons = cons_lock.lock();
                    cons.write_byte(b'\r');
                }
                ostd::early_print!("\r");
            }
            if let Some(cons_lock) = crate::drivers::drm::get_console() {
                let mut cons = cons_lock.lock();
                cons.write_byte(b);
            }
            ostd::early_print!("{}", b as char);
            written += 1;
        }

        Ok(written)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> Result<usize> {
        self.ldisc.ioctl(cmd, arg)
    }
}
