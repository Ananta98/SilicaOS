// SPDX-License-Identifier: GPL-2.0

//! Standard PCI Class, Subclass, and Programming Interface Constants.

pub mod class {
    pub const UNCLASSIFIED: u8 = 0x00;
    pub const MASS_STORAGE: u8 = 0x01;
    pub const NETWORK: u8 = 0x02;
    pub const DISPLAY: u8 = 0x03;
    pub const MULTIMEDIA: u8 = 0x04;
    pub const MEMORY: u8 = 0x05;
    pub const BRIDGE: u8 = 0x06;
    pub const SIMPLE_COMM: u8 = 0x07;
    pub const BASE_PERIPHERAL: u8 = 0x08;
    pub const INPUT: u8 = 0x09;
    pub const DOCKING: u8 = 0x0A;
    pub const PROCESSOR: u8 = 0x0B;
    pub const SERIAL_BUS: u8 = 0x0C;
    pub const WIRELESS: u8 = 0x0D;
    pub const INTELLIGENT_IO: u8 = 0x0E;
    pub const SATELLITE: u8 = 0x0F;
    pub const CRYPTO: u8 = 0x10;
    pub const SIGNAL_PROCESSING: u8 = 0x11;

    pub mod network {
        pub const ETHERNET: u8 = 0x00;
        pub const TOKEN_RING: u8 = 0x01;
        pub const FDDI: u8 = 0x02;
        pub const ATM: u8 = 0x03;
        pub const ISDN: u8 = 0x04;
        pub const WORLDFIP: u8 = 0x05;
        pub const PICMG: u8 = 0x06;
        pub const OTHER: u8 = 0x80;
    }
}

pub mod mass_storage {
    pub const SCSI: u8 = 0x00;
    pub const IDE: u8 = 0x01;
    pub const FLOPPY: u8 = 0x02;
    pub const IPI: u8 = 0x03;
    pub const RAID: u8 = 0x04;
    pub const ATA: u8 = 0x05;
    pub const SATA: u8 = 0x06;
    pub const SAS: u8 = 0x07;
    pub const NON_VOLATILE_MEMORY: u8 = 0x08;
    pub const OTHER: u8 = 0x80;

    pub mod nvm {
        pub const NVMHCI: u8 = 0x01;
        /// NVM Express (NVMe) controller interface
        pub const NVME: u8 = 0x02;
    }
}

pub mod bridge {
    pub const HOST: u8 = 0x00;
    pub const ISA: u8 = 0x01;
    pub const EISA: u8 = 0x02;
    pub const MCA: u8 = 0x03;
    pub const PCI_TO_PCI: u8 = 0x04;
    pub const PCMCIA: u8 = 0x05;
    pub const CARDBUS: u8 = 0x06;
    pub const OTHER: u8 = 0x80;
}
