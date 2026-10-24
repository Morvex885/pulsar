//! Byte order helpers for the Zigbee radio and deCONZ protocols: Zigbee and pulsar's 0x0Axx radio
//! messages are little endian, stock zbhci fields big endian. Readers take at least as many bytes
//! as they decode; callers check the length first.

pub fn put_le16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

pub fn put_le32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

pub fn put_le64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

pub fn get_le16(p: &[u8]) -> u16 {
    u16::from_le_bytes([p[0], p[1]])
}

pub fn get_le32(p: &[u8]) -> u32 {
    u32::from_le_bytes([p[0], p[1], p[2], p[3]])
}

pub fn get_le64(p: &[u8]) -> u64 {
    u64::from(get_le32(p)) | u64::from(get_le32(&p[4..])) << 32
}

pub fn get_be16(p: &[u8]) -> u16 {
    u16::from_be_bytes([p[0], p[1]])
}

pub fn get_be32(p: &[u8]) -> u32 {
    u32::from_be_bytes([p[0], p[1], p[2], p[3]])
}

pub fn get_be64(p: &[u8]) -> u64 {
    u64::from(get_be32(p)) << 32 | u64::from(get_be32(&p[4..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_orders() {
        let mut v = Vec::new();
        put_le16(&mut v, 0x1234);
        put_le32(&mut v, 0x89AB_CDEF);
        put_le64(&mut v, 0x0011_2233_4455_6677);
        assert_eq!(
            v,
            [
                0x34, 0x12, 0xEF, 0xCD, 0xAB, 0x89, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00
            ]
        );
        assert_eq!(get_le16(&v), 0x1234);
        assert_eq!(get_le32(&v[2..]), 0x89AB_CDEF);
        assert_eq!(get_le64(&v[6..]), 0x0011_2233_4455_6677);
        assert_eq!(get_be16(&v), 0x3412);
        assert_eq!(get_be32(&v[2..]), 0xEFCD_AB89);
        assert_eq!(get_be64(&v[6..]), 0x7766_5544_3322_1100);
    }
}
