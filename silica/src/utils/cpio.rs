use core::str;

#[derive(Debug)]
pub enum CpioError {
    InvalidMagic,
    InvalidHeader,
    TruncatedData,
}

#[derive(Debug, PartialEq)]
pub enum FileType {
    Socket,
    Symlink,
    File,
    BlockDevice,
    Directory,
    CharDevice,
    Fifo,
    Unknown,
}

#[derive(Debug)]
pub struct CpioEntry<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: u32,
    pub filesize: u32,
    pub file_type: FileType,
}

pub struct CpioArchive<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> CpioArchive<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }
    
    fn parse_hex(bytes: &[u8]) -> Result<u32, CpioError> {
        let s = str::from_utf8(bytes).map_err(|_| CpioError::InvalidHeader)?;
        u32::from_str_radix(s, 16).map_err(|_| CpioError::InvalidHeader)
    }
}

impl<'a> Iterator for CpioArchive<'a> {
    type Item = Result<CpioEntry<'a>, CpioError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset + 110 > self.data.len() {
            return None;
        }

        let header = &self.data[self.offset..self.offset + 110];
        // Support for newc cpio archives
        if &header[0..6] != b"070701" && &header[0..6] != b"070702" {
            return Some(Err(CpioError::InvalidMagic));
        }

        let mode = match Self::parse_hex(&header[14..22]) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let uid = match Self::parse_hex(&header[22..30]) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let gid = match Self::parse_hex(&header[30..38]) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let mtime = match Self::parse_hex(&header[46..54]) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let filesize = match Self::parse_hex(&header[54..62]) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let namesize = match Self::parse_hex(&header[94..102]) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };

        if self.offset + 110 + namesize as usize > self.data.len() {
            return Some(Err(CpioError::TruncatedData));
        }

        let name_bytes = &self.data[self.offset + 110..self.offset + 110 + (namesize as usize) - 1]; // -1 for NUL
        let name = match str::from_utf8(name_bytes) {
            Ok(n) => n,
            Err(_) => return Some(Err(CpioError::InvalidHeader)),
        };

        if name == "TRAILER!!!" {
            return None;
        }

        let data_offset = self.offset + 110 + namesize as usize;
        // align to 4 bytes
        let padding = (4 - (data_offset % 4)) % 4;
        let data_start = data_offset + padding;
        
        let data_end = data_start + filesize as usize;
        if data_end > self.data.len() {
            return Some(Err(CpioError::TruncatedData));
        }
        
        let file_type = match (mode & 0o170000) >> 12 {
            0o14 => FileType::Socket,
            0o12 => FileType::Symlink,
            0o10 => FileType::File,
            0o06 => FileType::BlockDevice,
            0o04 => FileType::Directory,
            0o02 => FileType::CharDevice,
            0o01 => FileType::Fifo,
            _ => FileType::Unknown,
        };

        let entry = CpioEntry {
            name,
            data: &self.data[data_start..data_end],
            mode,
            uid,
            gid,
            mtime,
            filesize,
            file_type,
        };

        let next_offset = data_end;
        let next_padding = (4 - (next_offset % 4)) % 4;
        self.offset = next_offset + next_padding;

        Some(Ok(entry))
    }
}
