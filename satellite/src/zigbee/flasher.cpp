#include "zigbee/flasher.h"

#include <fcntl.h>
#include <poll.h>
#include <termios.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <cstring>
#include <fstream>
#include <iterator>
#include <thread>

#include "core/log.h"
#include "hw/sysfs.h"

namespace cs {

namespace {

constexpr uint8_t SOH = 0x01, EOT = 0x04, ACK = 0x06, NAK = 0x15, CAN = 0x18;
constexpr uint16_t kManufacturer = 0x140A;  // Yandex, as in gateway.ybl
constexpr uint16_t kImageType = 0x0580;     // B91, gateway, bootloader layout
constexpr size_t kMaxImage = 0x77000 - 0x8000;  // application slot below the gateway's OTA area

uint32_t le32(const uint8_t *p) { return p[0] | p[1] << 8 | p[2] << 16 | uint32_t(p[3]) << 24; }

uint32_t crc32(const uint8_t *p, size_t n) {
  uint32_t c = 0xFFFFFFFFu;
  for (size_t i = 0; i < n; i++) {
    c ^= p[i];
    for (int k = 0; k < 8; k++) c = (c >> 1) ^ (0xEDB88320u & (0u - (c & 1)));
  }
  return ~c;
}

uint16_t crc16_xmodem(const uint8_t *p, size_t n) {
  uint16_t c = 0;
  for (size_t i = 0; i < n; i++) {
    c ^= uint16_t(p[i] << 8);
    for (int k = 0; k < 8; k++) c = (c & 0x8000) ? uint16_t(c << 1 ^ 0x1021) : uint16_t(c << 1);
  }
  return c;
}

void sleep_ms(int ms) { std::this_thread::sleep_for(std::chrono::milliseconds(ms)); }

class Port {
 public:
  explicit Port(const std::string &tty) {
    fd_ = ::open(tty.c_str(), O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (fd_ < 0) return;
    if (!isatty(fd_)) {  // never write menu keys or an image into a regular file
      ::close(fd_);
      fd_ = -1;
      errno = ENOTTY;
      return;
    }
    termios t{};
    tcgetattr(fd_, &t);
    cfmakeraw(&t);
    cfsetispeed(&t, B115200);
    cfsetospeed(&t, B115200);
    t.c_cflag |= CLOCAL | CREAD;
    t.c_cflag &= ~(CSTOPB | CRTSCTS);
    tcsetattr(fd_, TCSANOW, &t);
    tcflush(fd_, TCIOFLUSH);
  }
  ~Port() {
    if (fd_ >= 0) ::close(fd_);
  }
  bool ok() const { return fd_ >= 0; }
  bool put(const void *p, size_t n) {
    const uint8_t *b = static_cast<const uint8_t *>(p);
    while (n > 0) {
      ssize_t w = ::write(fd_, b, n);
      if (w < 0 && errno == EINTR) continue;
      if (w <= 0) return false;
      b += w;
      n -= size_t(w);
    }
    return true;
  }
  bool put(uint8_t c) { return put(&c, 1); }
  // Next byte within ms, or -1.
  int get(int ms) {
    pollfd p{fd_, POLLIN, 0};
    if (::poll(&p, 1, ms) <= 0) return -1;
    uint8_t c;
    return ::read(fd_, &c, 1) == 1 ? c : -1;
  }
  // Reads until `token` shows up (true) or nothing arrives for quiet_ms; what was read goes to *seen.
  bool expect(const char *token, int quiet_ms, std::string *seen) {
    for (;;) {
      int c = get(quiet_ms);
      if (c < 0) return false;
      if (c >= 0x20 || c == '\n') *seen += char(c);
      if (seen->find(token) != std::string::npos) return true;
    }
  }

 private:
  int fd_ = -1;
};

std::string printable(std::string s) {
  for (char &c : s)
    if (c == '\n') c = '|';
  return s;
}

}  // namespace

bool ybl_parse(const std::vector<uint8_t> &f, YblInfo *info, std::string *error) {
  auto fail = [&](const char *why) {
    if (error) *error = why;
    return false;
  };
  if (f.size() < 0x40 + 4) return fail("file too short");
  const uint32_t size = le32(f.data() + 0x18);
  if (size + 4 != f.size()) return fail("size field does not match the file");
  if (size > kMaxImage) return fail("image too large");
  if (memcmp(f.data() + 0x20, "KNLT", 4) != 0) return fail("no Telink start flag");
  if (crc32(f.data(), size) != le32(f.data() + size)) return fail("CRC-32 mismatch");
  YblInfo i;
  i.version = le32(f.data() + 2);
  i.manufacturer = uint16_t(f[0x12] | f[0x13] << 8);
  i.image_type = uint16_t(f[0x14] | f[0x15] << 8);
  if (i.manufacturer != kManufacturer || i.image_type != kImageType) return fail("not an image for this radio");
  if (info) *info = i;
  return true;
}

bool ybl_load(const std::string &path, std::vector<uint8_t> *file, YblInfo *info, std::string *error) {
  std::ifstream in(path, std::ios::binary);
  if (!in) {
    if (error) *error = "cannot read " + path;
    return false;
  }
  file->assign(std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>());
  if (!ybl_parse(*file, info, error)) {
    if (error) *error = path + ": " + *error;
    return false;
  }
  return true;
}

bool radio_flash(const RadioLines &lines, const std::vector<uint8_t> &image, const std::function<void(float)> &progress,
                 std::string *error) {
  auto fail = [&](const std::string &why) {
    if (error) *error = why;
    LOGE("zigbee flash: %s", why.c_str());
    return false;
  };
  Port port(lines.tty);
  if (!port.ok()) return fail("cannot open " + lines.tty + ": " + strerror(errno));

  // Bootloader entry exactly as maind (zb_control): boot and reset asserted, reset released after
  // 100 ms, boot 100 ms later.
  if (!hwutil::write_file(lines.boot, "1") || !hwutil::write_file(lines.reset, "1"))
    return fail("cannot drive the radio's reset/boot lines");
  sleep_ms(100);
  hwutil::write_file(lines.reset, "0");
  sleep_ms(100);
  hwutil::write_file(lines.boot, "0");

  std::string seen;
  bool menu = false;
  for (int i = 0; i < 3 && !menu; i++) {
    if (i) port.put('\n');
    menu = port.expect("BL >", 1000, &seen);
  }
  if (!menu) {
    // Not in the bootloader: the application keeps running (or a reset brings it back).
    hwutil::write_file(lines.reset, "1");
    sleep_ms(100);
    hwutil::write_file(lines.reset, "0");
    return fail("no bootloader prompt (got '" + printable(seen) + "')");
  }
  LOGI("zigbee flash: bootloader: %s", printable(seen).c_str());

  // "1. upload ybl", then XMODEM: the receiver asks with 'C' (CRC-16) or NAK (checksum).
  port.put('1');
  bool use_crc = false;
  const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
  for (;;) {
    int c = port.get(100);
    if (c == 'C' || c == NAK) {
      use_crc = c == 'C';
      break;
    }
    if (std::chrono::steady_clock::now() > deadline) {
      port.put('2');
      return fail("bootloader did not start XMODEM");
    }
  }

  const size_t blocks = (image.size() + 127) / 128;
  uint8_t pkt[133];
  for (size_t b = 0; b < blocks; b++) {
    const size_t off = b * 128, n = std::min<size_t>(128, image.size() - off);
    pkt[0] = SOH;
    pkt[1] = uint8_t(b + 1);
    pkt[2] = uint8_t(~pkt[1]);
    memcpy(pkt + 3, image.data() + off, n);
    memset(pkt + 3 + n, 0, 128 - n);
    if (n < 128) pkt[3 + n] = 0x1A;  // as maind pads the last block
    size_t len;
    if (use_crc) {
      const uint16_t crc = crc16_xmodem(pkt + 3, 128);
      pkt[131] = uint8_t(crc >> 8);
      pkt[132] = uint8_t(crc);
      len = 133;
    } else {
      uint8_t sum = 0;
      for (int i = 0; i < 128; i++) sum = uint8_t(sum + pkt[3 + i]);
      pkt[131] = sum;
      len = 132;
    }
    bool acked = false;
    for (int tries = 0; tries < 25 && !acked; tries++) {
      port.put(pkt, len);
      int c = port.get(3000);
      if (c == ACK) acked = true;
      else if (c == CAN && port.get(1000) == CAN) return fail("bootloader cancelled the transfer at block " + std::to_string(b + 1));
    }
    if (!acked) {
      for (int i = 0; i < 3; i++) port.put(CAN);
      return fail("block " + std::to_string(b + 1) + " not acknowledged");
    }
    if (progress) progress(float(b + 1) / float(blocks));
  }
  bool done = false;
  for (int tries = 0; tries < 10 && !done; tries++) {
    port.put(EOT);
    done = port.get(2000) == ACK;
  }
  if (!done) return fail("end of transfer not acknowledged");

  seen.clear();
  port.expect("BL >", 5000, &seen);
  LOGI("zigbee flash: upload done: %s", printable(seen).c_str());
  port.put('2');  // run
  return true;
}

}  // namespace cs
