// SPDX-License-Identifier: GPL-2.0

//! Intel e1000 (8254x / 82574L) Register Definitions and Bitfields.

pub const REG_CTRL: usize = 0x0000;
pub const REG_STATUS: usize = 0x0008;
pub const REG_EECD: usize = 0x0010;
pub const REG_EERD: usize = 0x0014;
pub const REG_CTRL_EXT: usize = 0x0018;
pub const REG_MDIC: usize = 0x0020;
pub const REG_ICR: usize = 0x00C0;
pub const REG_ITR: usize = 0x00C4;
pub const REG_ICS: usize = 0x00C8;
pub const REG_IMS: usize = 0x00D0;
pub const REG_IMC: usize = 0x00D8;
pub const REG_RCTL: usize = 0x0100;
pub const REG_TCTL: usize = 0x0400;
pub const REG_TIPG: usize = 0x0410;
pub const REG_RDBAL: usize = 0x2800;
pub const REG_RDBAH: usize = 0x2804;
pub const REG_RDLEN: usize = 0x2808;
pub const REG_RDH: usize = 0x2810;
pub const REG_RDT: usize = 0x2818;
pub const REG_RDTR: usize = 0x2820;
pub const REG_TDBAL: usize = 0x3800;
pub const REG_TDBAH: usize = 0x3804;
pub const REG_TDLEN: usize = 0x3808;
pub const REG_TDH: usize = 0x3810;
pub const REG_TDT: usize = 0x3818;
pub const REG_TIDV: usize = 0x3820;
pub const REG_MTA: usize = 0x5200;
pub const REG_RAL: usize = 0x5400;
pub const REG_RAH: usize = 0x5404;

// CTRL bitfields
pub const CTRL_FD: u32 = 1 << 0;         // Full Duplex
pub const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
pub const CTRL_ASDE: u32 = 1 << 5;       // Auto-Speed Detection Enable
pub const CTRL_SLU: u32 = 1 << 6;        // Set Link Up
pub const CTRL_RST: u32 = 1 << 26;       // Device Reset
pub const CTRL_VME: u32 = 1 << 30;       // VLAN Mode Enable
pub const CTRL_PHY_RST: u32 = 1 << 31;   // PHY Reset

// STATUS bitfields
pub const STATUS_FD: u32 = 1 << 0;       // Full Duplex
pub const STATUS_LU: u32 = 1 << 1;       // Link Up
pub const STATUS_SPEED_1000: u32 = 2 << 6;

// RCTL bitfields
pub const RCTL_EN: u32 = 1 << 1;         // Receiver Enable
pub const RCTL_SBP: u32 = 1 << 2;        // Store Bad Packets
pub const RCTL_UPE: u32 = 1 << 3;        // Unicast Promiscuous Enable
pub const RCTL_MPE: u32 = 1 << 4;        // Multicast Promiscuous Enable
pub const RCTL_LPE: u32 = 1 << 5;        // Long Packet Enable
pub const RCTL_LBM_NONE: u32 = 0 << 6;   // No Loopback
pub const RCTL_RDMTS_HALF: u32 = 0 << 8; // Rx Descriptor Min Threshold Size
pub const RCTL_MO_36: u32 = 0 << 12;     // Multicast Offset
pub const RCTL_BAM: u32 = 1 << 15;       // Broadcast Accept Mode
pub const RCTL_SZ_2048: u32 = 0 << 16;   // Buffer size 2048
pub const RCTL_SECRC: u32 = 1 << 26;     // Strip Ethernet CRC

// TCTL bitfields
pub const TCTL_EN: u32 = 1 << 1;         // Transmit Enable
pub const TCTL_PSP: u32 = 1 << 3;        // Pad Short Packets
pub const TCTL_CT_SHIFT: u32 = 4;        // Collision Threshold (default 15)
pub const TCTL_COLD_SHIFT: u32 = 12;     // Collision Distance (default 64)
pub const TCTL_SWXOFF: u32 = 1 << 22;    // Software XOFF Transmission
pub const TCTL_RTLC: u32 = 1 << 24;      // Re-transmit on Late Collision

// EERD bitfields
pub const EERD_START: u32 = 1 << 0;      // Start Read
pub const EERD_DONE: u32 = 1 << 4;       // Read Done
pub const EERD_ADDR_SHIFT: u32 = 8;
pub const EERD_DATA_SHIFT: u32 = 16;

// RX descriptor status bits
pub const RDESC_STAT_DD: u8 = 1 << 0;    // Descriptor Done
pub const RDESC_STAT_EOP: u8 = 1 << 1;   // End of Packet

// TX descriptor command bits
pub const TDESC_CMD_EOP: u8 = 1 << 0;    // End of Packet
pub const TDESC_CMD_IFCS: u8 = 1 << 1;   // Insert FCS (CRC)
pub const TDESC_CMD_RS: u8 = 1 << 3;     // Report Status
pub const TDESC_STAT_DD: u8 = 1 << 0;    // Descriptor Done
