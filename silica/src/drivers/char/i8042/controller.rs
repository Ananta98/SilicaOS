// SPDX-License-Identifier: GPL-2.0

//! i8042 PS/2 Microcontroller Driver and Port Abstraction.
//!
//! Controls the Intel 8042 / standard motherboard keyboard and mouse
//! microcontroller via I/O ports 0x60 and 0x64.

use bitflags::bitflags;
use ostd::{
    arch::device::io_port::ReadWriteAccess,
    io::IoPort,
    sync::{LocalIrqDisabled, SpinLock},
};
use spin::Once;

use crate::errno::{Errno, Result};

// ----------------------------------------------------------------------------
// I/O Port Addresses
// ----------------------------------------------------------------------------

pub const I8042_DATA_PORT: u16 = 0x60;
pub const I8042_STATUS_PORT: u16 = 0x64;
pub const I8042_COMMAND_PORT: u16 = 0x64;

// ----------------------------------------------------------------------------
// Controller Commands
// ----------------------------------------------------------------------------

pub const CMD_READ_CONFIG: u8 = 0x20;
pub const CMD_WRITE_CONFIG: u8 = 0x60;
pub const CMD_DISABLE_PORT2: u8 = 0xA7;
pub const CMD_ENABLE_PORT2: u8 = 0xA8;
pub const CMD_TEST_PORT2: u8 = 0xA9;
pub const CMD_TEST_CONTROLLER: u8 = 0xAA;
pub const CMD_TEST_PORT1: u8 = 0xAB;
pub const CMD_DISABLE_PORT1: u8 = 0xAD;
pub const CMD_ENABLE_PORT1: u8 = 0xAE;
pub const CMD_WRITE_PORT2_INPUT: u8 = 0xD4;

// ----------------------------------------------------------------------------
// Status Register Bitflags
// ----------------------------------------------------------------------------

bitflags! {
    /// Status register bits read from port 0x64.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct StatusFlags: u8 {
        /// Output buffer full (1 = data waiting to be read from port 0x60).
        const OUTPUT_FULL  = 1 << 0;
        /// Input buffer full (1 = controller busy processing written data/command).
        const INPUT_FULL   = 1 << 1;
        /// System Flag (1 = system passed power-on self-test).
        const SYSTEM_FLAG  = 1 << 2;
        /// Command/Data (0 = data written to 0x60 was device data; 1 = was for controller).
        const COMMAND_DATA = 1 << 3;
        /// Keyboard lock switch (0 = locked; 1 = unlocked).
        const KEYBOARD_LOCK = 1 << 4;
        /// Auxiliary output buffer full (1 = data in 0x60 came from Port 2 / mouse).
        const AUX_FULL     = 1 << 5;
        /// Time-out error.
        const TIMEOUT_ERR  = 1 << 6;
        /// Parity error.
        const PARITY_ERR   = 1 << 7;
    }
}

// ----------------------------------------------------------------------------
// Controller Configuration Register Bitflags
// ----------------------------------------------------------------------------

bitflags! {
    /// Controller Configuration Byte read/written via commands 0x20 / 0x60.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ConfigFlags: u8 {
        /// Port 1 (Keyboard) interrupt enabled (IRQ 1).
        const PORT1_INT_ENABLE   = 1 << 0;
        /// Port 2 (Mouse) interrupt enabled (IRQ 12).
        const PORT2_INT_ENABLE   = 1 << 1;
        /// System Flag.
        const SYSTEM_FLAG        = 1 << 2;
        /// Zero / reserved.
        const ZERO_FLAG          = 1 << 3;
        /// Port 1 clock disabled (1 = disabled).
        const PORT1_CLOCK_DISABLE = 1 << 4;
        /// Port 2 clock disabled (1 = disabled).
        const PORT2_CLOCK_DISABLE = 1 << 5;
        /// Port 1 translation enabled (translates Set 2 scancodes to Set 1).
        const PORT1_TRANSLATION   = 1 << 6;
        /// Reserved bit 7.
        const RESERVED           = 1 << 7;
    }
}

// ----------------------------------------------------------------------------
// i8042 Controller Abstraction
// ----------------------------------------------------------------------------

const MAX_SPIN_TIMEOUT: usize = 100_000;

/// Interface to the physical i8042 PS/2 controller.
pub struct I8042Controller {
    data_port: IoPort<u8, ReadWriteAccess>,
    cmd_port: IoPort<u8, ReadWriteAccess>,
    has_second_port: bool,
}

impl I8042Controller {
    /// Acquires the controller I/O ports and initializes the structure.
    pub fn new() -> Result<Self> {
        let data_port = IoPort::<u8, ReadWriteAccess>::acquire_overlapping(I8042_DATA_PORT)
            .map_err(|_| Errno::EBUSY)?;
        let cmd_port = IoPort::<u8, ReadWriteAccess>::acquire_overlapping(I8042_COMMAND_PORT)
            .map_err(|_| Errno::EBUSY)?;

        Ok(Self {
            data_port,
            cmd_port,
            has_second_port: false,
        })
    }

    /// Reads the current status flags from port 0x64.
    pub fn read_status(&self) -> StatusFlags {
        StatusFlags::from_bits_truncate(self.cmd_port.read())
    }

    /// Reads raw data byte from port 0x60 directly.
    pub fn read_data(&self) -> u8 {
        self.data_port.read()
    }

    /// Checks if the output buffer has data available to be read from port 0x60.
    pub fn can_read(&self) -> bool {
        self.read_status().contains(StatusFlags::OUTPUT_FULL)
    }

    /// Checks if the input buffer is empty and ready to accept writes.
    pub fn can_write(&self) -> bool {
        !self.read_status().contains(StatusFlags::INPUT_FULL)
    }

    /// Flushes any pending residual bytes from the output buffer.
    pub fn flush_buffer(&self) {
        for _ in 0..64 {
            if self.read_status().contains(StatusFlags::OUTPUT_FULL) {
                let _ = self.data_port.read();
            } else {
                break;
            }
        }
    }

    /// Polls until data is available in the output buffer and reads it.
    pub fn wait_and_read_data(&self) -> Result<u8> {
        for _ in 0..MAX_SPIN_TIMEOUT {
            if self.read_status().contains(StatusFlags::OUTPUT_FULL) {
                return Ok(self.data_port.read());
            }
            core::hint::spin_loop();
        }
        crate::return_errno!(EIO, "i8042: read timeout waiting for data");
    }

    /// Polls until the controller is ready to receive a command and writes it to port 0x64.
    pub fn wait_and_send_cmd(&self, cmd: u8) -> Result<()> {
        for _ in 0..MAX_SPIN_TIMEOUT {
            if !self.read_status().contains(StatusFlags::INPUT_FULL) {
                self.cmd_port.write(cmd);
                return Ok(());
            }
            core::hint::spin_loop();
        }
        crate::return_errno!(EIO, "i8042: write timeout sending command");
    }

    /// Polls until the controller is ready to receive data and writes it to port 0x60.
    pub fn wait_and_send_data(&self, data: u8) -> Result<()> {
        for _ in 0..MAX_SPIN_TIMEOUT {
            if !self.read_status().contains(StatusFlags::INPUT_FULL) {
                self.data_port.write(data);
                return Ok(());
            }
            core::hint::spin_loop();
        }
        crate::return_errno!(EIO, "i8042: write timeout sending data");
    }

    /// Directs the next byte written to port 0x60 to the secondary PS/2 port (Mouse).
    pub fn send_to_port2(&self, data: u8) -> Result<()> {
        self.wait_and_send_cmd(CMD_WRITE_PORT2_INPUT)?;
        self.wait_and_send_data(data)?;
        Ok(())
    }

    /// Reads the controller configuration byte.
    pub fn read_config(&self) -> Result<ConfigFlags> {
        self.wait_and_send_cmd(CMD_READ_CONFIG)?;
        let val = self.wait_and_read_data()?;
        Ok(ConfigFlags::from_bits_truncate(val))
    }

    /// Writes a new controller configuration byte.
    pub fn write_config(&self, cfg: ConfigFlags) -> Result<()> {
        self.wait_and_send_cmd(CMD_WRITE_CONFIG)?;
        self.wait_and_send_data(cfg.bits())?;
        Ok(())
    }

    /// Returns whether the controller detected a functional second port.
    pub fn has_second_port(&self) -> bool {
        self.has_second_port
    }
}

/// Global singleton instance of the active i8042 controller protected by IRQ-safe SpinLock.
pub static I8042_CONTROLLER: Once<SpinLock<I8042Controller, LocalIrqDisabled>> = Once::new();

/// Probes and initializes the i8042 PS/2 controller, bringing up keyboard and mouse.
pub fn init() -> Result<()> {
    ostd::info!("i8042: Initializing PS/2 controller...");

    let mut controller = I8042Controller::new()?;

    // 1. Disable first and second ports during setup to prevent spurious interrupts
    let _ = controller.wait_and_send_cmd(CMD_DISABLE_PORT1);
    let _ = controller.wait_and_send_cmd(CMD_DISABLE_PORT2);

    // 2. Flush residual buffer contents
    controller.flush_buffer();

    // 3. Read configuration and disable interrupts & clock lines
    let mut config = controller.read_config().unwrap_or(ConfigFlags::empty());
    config.remove(ConfigFlags::PORT1_INT_ENABLE | ConfigFlags::PORT2_INT_ENABLE);
    // Keep hardware translation enabled so Set 2 is automatically mapped to Set 1
    config.insert(ConfigFlags::PORT1_TRANSLATION);
    let _ = controller.write_config(config);

    // 4. Controller self-test
    const SELF_TEST_OK: u8 = 0x55;
    if controller.wait_and_send_cmd(CMD_TEST_CONTROLLER).is_ok() {
        if let Ok(result) = controller.wait_and_read_data() {
            if result != SELF_TEST_OK {
                ostd::warn!("i8042: Controller self-test returned 0x{:02X} (expected 0x55)", result);
            } else {
                ostd::info!("i8042: Controller self-test passed (0x55)");
            }
        }
    }
    // Controller self-test may reset configuration; restore our configuration
    let _ = controller.write_config(config);
    controller.flush_buffer();

    // 5. Test if dual-channel (secondary PS/2 port for mouse) is supported
    if controller.wait_and_send_cmd(CMD_ENABLE_PORT2).is_ok() {
        if let Ok(cfg_after) = controller.read_config() {
            if !cfg_after.contains(ConfigFlags::PORT2_CLOCK_DISABLE) {
                controller.has_second_port = true;
                ostd::info!("i8042: Dual-channel PS/2 controller detected (Port 2 mouse supported)");
            }
        }
        let _ = controller.wait_and_send_cmd(CMD_DISABLE_PORT2);
    }
    controller.flush_buffer();

    // 6. Enable Port 1 (Keyboard) and initialize keyboard driver
    let _ = controller.wait_and_send_cmd(CMD_ENABLE_PORT1);
    config.remove(ConfigFlags::PORT1_CLOCK_DISABLE);
    config.insert(ConfigFlags::PORT1_INT_ENABLE);

    if let Err(err) = super::keyboard::init(&controller) {
        ostd::warn!("i8042: Keyboard initialization failed: {:?}", err);
    }

    // 7. Enable Port 2 (Mouse) if present and initialize mouse driver
    if controller.has_second_port {
        let _ = controller.wait_and_send_cmd(CMD_ENABLE_PORT2);
        config.remove(ConfigFlags::PORT2_CLOCK_DISABLE);
        config.insert(ConfigFlags::PORT2_INT_ENABLE);

        if let Err(err) = super::mouse::init(&controller) {
            ostd::warn!("i8042: Mouse initialization failed: {:?}", err);
        }
    }

    // 8. Write final configuration enabling interrupts
    if let Err(err) = controller.write_config(config) {
        ostd::warn!("i8042: Failed to write final controller configuration: {:?}", err);
    }

    ostd::info!("i8042: PS/2 Controller initialization complete");

    I8042_CONTROLLER.call_once(|| SpinLock::new(controller));

    Ok(())
}
