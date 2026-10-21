// Minimal hand-written protobuf (proto3) wire codec for the ESPHome native API.
#pragma once

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

namespace cs {
namespace pb {

enum WireType : uint8_t { VARINT = 0, FIXED64 = 1, LEN = 2, FIXED32 = 5 };

inline void put_varint(std::string &out, uint64_t v) {
  while (v >= 0x80) {
    out.push_back(char(uint8_t(v) | 0x80));
    v >>= 7;
  }
  out.push_back(char(uint8_t(v)));
}

// Decodes a varint at p[*pos..n). Returns false if truncated or longer than 10 bytes.
inline bool get_varint(const uint8_t *p, size_t n, size_t *pos, uint64_t *out) {
  uint64_t v = 0;
  for (int shift = 0; shift < 70 && *pos < n; shift += 7) {
    uint8_t b = p[(*pos)++];
    v |= uint64_t(b & 0x7f) << shift;
    if (!(b & 0x80)) {
      *out = v;
      return true;
    }
  }
  return false;
}

// Encoder. Proto3 semantics: scalar fields equal to their default are omitted unless forced.
class Writer {
 public:
  const std::string &data() const { return buf_; }
  std::string &data() { return buf_; }
  size_t size() const { return buf_.size(); }

  void tag(uint32_t field, WireType wt) { put_varint(buf_, (uint64_t(field) << 3) | wt); }

  void u32(uint32_t field, uint32_t v, bool force = false) {
    if (!v && !force) return;
    tag(field, VARINT);
    put_varint(buf_, v);
  }
  void i32(uint32_t field, int32_t v, bool force = false) {
    if (!v && !force) return;
    tag(field, VARINT);
    put_varint(buf_, uint64_t(int64_t(v)));  // negative int32 is sign-extended to 10 bytes
  }
  void boolean(uint32_t field, bool v, bool force = false) {
    if (!v && !force) return;
    tag(field, VARINT);
    buf_.push_back(v ? 1 : 0);
  }
  void fixed32(uint32_t field, uint32_t v, bool force = false) {
    if (!v && !force) return;
    tag(field, FIXED32);
    for (int i = 0; i < 4; i++) buf_.push_back(char(uint8_t(v >> (8 * i))));
  }
  void flt(uint32_t field, float v, bool force = false) {
    if (v == 0.0f && !force) return;
    uint32_t u;
    memcpy(&u, &v, 4);
    fixed32(field, u, true);
  }
  void bytes(uint32_t field, const void *p, size_t n, bool force = false) {
    if (!n && !force) return;
    tag(field, LEN);
    put_varint(buf_, n);
    buf_.append(static_cast<const char *>(p), n);
  }
  void str(uint32_t field, const std::string &s, bool force = false) { bytes(field, s.data(), s.size(), force); }
  void msg(uint32_t field, const Writer &sub) { bytes(field, sub.buf_.data(), sub.buf_.size(), true); }
  // Packed repeated varints (canonical proto3 encoding for repeated enums/uint32).
  void packed_u32(uint32_t field, const std::vector<uint32_t> &vals) {
    if (vals.empty()) return;
    std::string tmp;
    for (uint32_t v : vals) put_varint(tmp, v);
    bytes(field, tmp.data(), tmp.size(), true);
  }

 private:
  std::string buf_;
};

// One decoded field. For LEN fields, data/len point into the reader's buffer.
struct Field {
  uint32_t num = 0;
  WireType wt = VARINT;
  uint64_t v = 0;  // VARINT / FIXED32 / FIXED64 value
  const uint8_t *data = nullptr;
  size_t len = 0;

  uint32_t u32() const { return uint32_t(v); }
  bool boolean() const { return v != 0; }
  float flt() const {
    uint32_t u = uint32_t(v);
    float f;
    memcpy(&f, &u, 4);
    return f;
  }
  std::string str() const { return std::string(reinterpret_cast<const char *>(data), len); }
};

class Reader {
 public:
  Reader(const uint8_t *p, size_t n) : p_(p), n_(n) {}
  Reader(const std::string &s) : p_(reinterpret_cast<const uint8_t *>(s.data())), n_(s.size()) {}
  explicit Reader(const Field &f) : p_(f.data), n_(f.len) {}

  // Returns false at end of buffer or on malformed input (then error() is true).
  bool next(Field &f) {
    if (pos_ >= n_) return false;
    uint64_t key;
    if (!get_varint(p_, n_, &pos_, &key)) return fail();
    f.num = uint32_t(key >> 3);
    f.wt = WireType(key & 7);
    f.data = nullptr;
    f.len = 0;
    f.v = 0;
    switch (f.wt) {
      case VARINT:
        if (!get_varint(p_, n_, &pos_, &f.v)) return fail();
        return true;
      case FIXED64:
        if (n_ - pos_ < 8) return fail();
        for (int i = 0; i < 8; i++) f.v |= uint64_t(p_[pos_ + i]) << (8 * i);
        pos_ += 8;
        return true;
      case FIXED32:
        if (n_ - pos_ < 4) return fail();
        for (int i = 0; i < 4; i++) f.v |= uint64_t(p_[pos_ + i]) << (8 * i);
        pos_ += 4;
        return true;
      case LEN: {
        uint64_t len;
        if (!get_varint(p_, n_, &pos_, &len) || len > n_ - pos_) return fail();
        f.data = p_ + pos_;
        f.len = size_t(len);
        pos_ += f.len;
        return true;
      }
      default:
        return fail();
    }
  }
  bool error() const { return err_; }

 private:
  bool fail() {
    err_ = true;
    pos_ = n_;
    return false;
  }
  const uint8_t *p_;
  size_t n_;
  size_t pos_ = 0;
  bool err_ = false;
};

}  // namespace pb
}  // namespace cs
