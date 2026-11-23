//! The bytecode file header.

// Magic bytes at the start of compiled bytecode binary
pub const MAGIC: &[u8; 7] = b"neyuki!";
/// Oldest bytecode format version this VM still decodes. Formats 7 and 8 only
/// append opcodes (7: immediate integer ops, tags 55..=65; 8: constant-operand
/// super-instructions `AddK`..`GeK`, tags 66..=74) without altering any older
/// opcode or prototype layout, so v6 and v7 `.nykb` files stay loadable. The
/// deserializer rejects a tag newer than the file's declared version.
pub const MIN_SUPPORTED_BYTECODE_VERSION: u8 = 6;
pub const BYTECODE_VERSION: u8 = 9;

#[inline]
pub fn is_supported_version(version: u8) -> bool {
    (MIN_SUPPORTED_BYTECODE_VERSION..=BYTECODE_VERSION).contains(&version)
}

// IEEE 802.3 CRC-32 checksum calculation for bytecode integrity verification
pub fn compute_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BytecodeHeader {
    pub version: u8,
    pub checksum: u32,
}

impl BytecodeHeader {
    pub const HEADER_SIZE: usize = 12;

    pub fn new(checksum: u32) -> Self {
        Self {
            version: BYTECODE_VERSION,
            checksum,
        }
    }

    pub fn write_to(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(MAGIC);
        buf.push(self.version);
        buf.extend_from_slice(&self.checksum.to_le_bytes());
    }

    pub fn parse(bytes: &[u8]) -> Result<(Self, usize), String> {
        if bytes.len() < Self::HEADER_SIZE {
            return Err("bytecode buffer too small to contain header".to_string());
        }
        if &bytes[0..7] != MAGIC {
            return Err("invalid bytecode magic bytes: expected 'neyuki!'".to_string());
        }
        let version = bytes[7];
        if !is_supported_version(version) {
            return Err(format!(
                "unsupported bytecode version: {} (expected {}..={})",
                version, MIN_SUPPORTED_BYTECODE_VERSION, BYTECODE_VERSION
            ));
        }
        let checksum = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        Ok((BytecodeHeader { version, checksum }, Self::HEADER_SIZE))
    }
}

impl Default for BytecodeHeader {
    fn default() -> Self {
        Self::new(0)
    }
}
