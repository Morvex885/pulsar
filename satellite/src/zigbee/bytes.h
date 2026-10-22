// Byte order helpers for the Zigbee radio and deCONZ protocols: Zigbee and pulsar's 0x0Axx radio
// messages are little endian, stock zbhci fields big endian.
#pragma once

#include <cstddef>
#include <cstdint>
#include <vector>

namespace cs {

inline void put_le16(std::vector<uint8_t> &v, uint16_t x) {
  v.push_back(uint8_t(x));
  v.push_back(uint8_t(x >> 8));
}
inline void put_le32(std::vector<uint8_t> &v, uint32_t x) {
  put_le16(v, uint16_t(x));
  put_le16(v, uint16_t(x >> 16));
}
inline void put_le64(std::vector<uint8_t> &v, uint64_t x) {
  put_le32(v, uint32_t(x));
  put_le32(v, uint32_t(x >> 32));
}
inline uint16_t get_le16(const uint8_t *p) { return uint16_t(p[0] | p[1] << 8); }
inline uint32_t get_le32(const uint8_t *p) { return uint32_t(get_le16(p)) | uint32_t(get_le16(p + 2)) << 16; }
inline uint64_t get_le64(const uint8_t *p) { return uint64_t(get_le32(p)) | uint64_t(get_le32(p + 4)) << 32; }

inline uint16_t get_be16(const uint8_t *p) { return uint16_t(p[0] << 8 | p[1]); }
inline uint32_t get_be32(const uint8_t *p) { return uint32_t(get_be16(p)) << 16 | get_be16(p + 2); }
inline uint64_t get_be64(const uint8_t *p) { return uint64_t(get_be32(p)) << 32 | get_be32(p + 4); }

}  // namespace cs
