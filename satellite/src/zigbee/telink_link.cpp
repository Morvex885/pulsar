#include "zigbee/telink_link.h"

#include <fcntl.h>
#include <sys/epoll.h>
#include <termios.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>

#include "core/log.h"
#include "hw/sysfs.h"

namespace cs {

namespace {

constexpr uint16_t kAck = 0x8000;
constexpr uint16_t kBootInd = 0x8907;   // sent by the application once it runs
constexpr uint16_t kCrashInd = 0x8909;  // crash report, then a restart
constexpr int64_t kAckTimeoutMs = 300;  // the ACK normally follows within a few ms
constexpr int kTries = 3;
constexpr int kFailuresBeforeReset = 3;
constexpr int64_t kResetPulseMs = 200;
constexpr int64_t kBootWaitMs = 3000;
constexpr size_t kMaxFrame = 0xF9;      // payload limit of the firmware's transmit path
constexpr size_t kMaxPayload = 0x1100;  // the crash report carries up to 4 KiB of RAM

}  // namespace

TelinkLink::TelinkLink(Loop &loop, std::string tty, std::string reset_line, std::string boot_line)
    : loop_(loop), tty_(std::move(tty)), reset_line_(std::move(reset_line)), boot_line_(std::move(boot_line)) {}

TelinkLink::~TelinkLink() {
  for (uint64_t t : {ack_timer_, boot_timer_, hold_timer_})
    if (t) loop_.cancel_timer(t);
  if (reset_held_) hwutil::write_file(reset_line_, "0");  // never leave the radio held in reset
  if (fd_ >= 0) {
    loop_.del_fd(fd_);
    ::close(fd_);
  }
}

bool TelinkLink::open() {
  fd_ = ::open(tty_.c_str(), O_RDWR | O_NOCTTY | O_NONBLOCK | O_CLOEXEC);
  if (fd_ < 0) {
    LOGE("zigbee: open %s: %s", tty_.c_str(), strerror(errno));
    return false;
  }
  termios t{};
  tcgetattr(fd_, &t);
  cfmakeraw(&t);
  cfsetispeed(&t, B115200);
  cfsetospeed(&t, B115200);
  t.c_cflag |= CLOCAL | CREAD;
  t.c_cflag &= ~(CSTOPB | CRTSCTS);
  t.c_cc[VMIN] = 0;
  t.c_cc[VTIME] = 0;
  if (tcsetattr(fd_, TCSANOW, &t) != 0) {
    LOGE("zigbee: configure %s: %s", tty_.c_str(), strerror(errno));
    ::close(fd_);
    fd_ = -1;
    return false;
  }
  tcflush(fd_, TCIOFLUSH);
  loop_.add_fd(fd_, EPOLLIN, [this](uint32_t) { on_readable(); });
  reset();
  return true;
}

void TelinkLink::send(uint16_t type, std::vector<uint8_t> payload, AckCb done, uint16_t hold_for, int64_t hold_ms) {
  Out o;
  o.type = type;
  o.done = std::move(done);
  o.hold_for = hold_for;
  o.hold_ms = hold_ms;
  const size_t n = payload.size();
  o.frame.reserve(n + 7);
  o.frame.push_back(0x55);
  o.frame.push_back(uint8_t(type >> 8));
  o.frame.push_back(uint8_t(type));
  o.frame.push_back(uint8_t(n >> 8));
  o.frame.push_back(uint8_t(n));
  uint8_t x = uint8_t(type >> 8) ^ uint8_t(type) ^ uint8_t(n >> 8) ^ uint8_t(n);
  for (uint8_t b : payload) x ^= b;
  o.frame.push_back(x);
  o.frame.insert(o.frame.end(), payload.begin(), payload.end());
  o.frame.push_back(0xAA);
  out_.push_back(std::move(o));
  pump();
}

void TelinkLink::reset() {
  for (uint64_t *t : {&ack_timer_, &boot_timer_, &hold_timer_}) {
    if (*t) loop_.cancel_timer(*t);
    *t = 0;
  }
  in_flight_ = false;
  hold_for_ = 0;
  resetting_ = true;
  std::deque<Out> dropped;
  dropped.swap(out_);
  for (auto &o : dropped)
    if (o.done) o.done(-1);

  // Boot line released: run the application, not the resident bootloader. Then a reset pulse.
  if (!hwutil::write_file(boot_line_, "0") || !hwutil::write_file(reset_line_, "1")) {
    LOGW("zigbee: cannot drive %s / %s, radio not reset", reset_line_.c_str(), boot_line_.c_str());
    boot_timer_ = loop_.add_timer(0, [this] {
      boot_timer_ = 0;
      booted();
    });
    return;
  }
  reset_held_ = true;
  boot_timer_ = loop_.add_timer(kResetPulseMs, [this] {
    hwutil::write_file(reset_line_, "0");
    reset_held_ = false;
    rx_.clear();
    boot_timer_ = loop_.add_timer(kBootWaitMs, [this] {
      boot_timer_ = 0;
      LOGW("zigbee: no boot indication from the radio, continuing");
      booted();
    });
  });
}

void TelinkLink::booted() {
  if (boot_timer_) loop_.cancel_timer(boot_timer_);
  boot_timer_ = 0;
  resetting_ = false;
  failures_ = 0;
  if (boot_cb_) boot_cb_();
  pump();
}

void TelinkLink::pump() {
  if (in_flight_ || resetting_ || hold_for_ || out_.empty() || fd_ < 0) return;
  Out &o = out_.front();
  o.tries++;
  const uint8_t *p = o.frame.data();
  size_t left = o.frame.size();
  while (left > 0) {
    ssize_t w = ::write(fd_, p, left);
    if (w < 0 && errno == EINTR) continue;
    if (w <= 0) break;  // kernel tty buffer full: the ACK timeout retries the frame
    p += w;
    left -= size_t(w);
  }
  in_flight_ = true;
  ack_timer_ = loop_.add_timer(kAckTimeoutMs, [this] {
    ack_timer_ = 0;
    ack_timeout();
  });
}

void TelinkLink::ack_timeout() {
  in_flight_ = false;
  if (out_.empty()) return;
  if (out_.front().tries < kTries) return pump();
  LOGW("zigbee: no ACK for frame 0x%04x", out_.front().type);
  if (++failures_ >= kFailuresBeforeReset) {
    LOGE("zigbee: radio not answering, resetting it");
    return reset();
  }
  finish_head(-1);
}

void TelinkLink::finish_head(int status) {
  if (ack_timer_) loop_.cancel_timer(ack_timer_);
  ack_timer_ = 0;
  in_flight_ = false;
  if (out_.empty()) return;
  Out o = std::move(out_.front());
  out_.pop_front();
  if (status >= 0) failures_ = 0;
  if (status == 0 && o.hold_for) {
    hold_for_ = o.hold_for;
    hold_timer_ = loop_.add_timer(o.hold_ms, [this] {
      hold_timer_ = 0;
      LOGW("zigbee: no 0x%04x from the radio", hold_for_);
      release_hold();
    });
  }
  if (o.done) o.done(status);
  pump();
}

void TelinkLink::release_hold() {
  if (hold_timer_) loop_.cancel_timer(hold_timer_);
  hold_timer_ = 0;
  hold_for_ = 0;
  pump();
}

void TelinkLink::on_readable() {
  uint8_t buf[512];
  for (;;) {
    ssize_t n = ::read(fd_, buf, sizeof buf);
    if (n > 0) {
      rx_.insert(rx_.end(), buf, buf + n);
      continue;
    }
    if (n < 0 && errno == EINTR) continue;
    break;
  }
  parse();
}

void TelinkLink::parse() {
  size_t pos = 0;
  while (rx_.size() - pos >= 7) {
    if (rx_[pos] != 0x55) {
      pos++;
      continue;
    }
    const uint8_t *h = rx_.data() + pos;
    const uint16_t type = uint16_t(h[1] << 8 | h[2]);
    const size_t len = size_t(h[3] << 8 | h[4]);
    // Normal frames carry at most 0xF9 bytes; only the crash report is longer. A stray 0x55 with a
    // bogus length must not hold back the frames behind it.
    if (len > (type == kCrashInd ? kMaxPayload : kMaxFrame)) {
      pos++;
      continue;
    }
    if (rx_.size() - pos < len + 7) break;
    uint8_t x = h[1] ^ h[2] ^ h[3] ^ h[4];
    for (size_t i = 0; i < len; i++) x ^= h[6 + i];
    if (h[6 + len] != 0xAA || x != h[5]) {
      pos++;
      continue;
    }
    pos += len + 7;
    // h points into rx_: handlers must not touch rx_ (reset() clears it only from a timer).
    handle(type, h + 6, len);
  }
  rx_.erase(rx_.begin(), rx_.begin() + long(pos));
}

void TelinkLink::handle(uint16_t type, const uint8_t *p, size_t n) {
  if (type == kAck) {
    if (n < 3 || !in_flight_ || out_.empty()) return;
    const uint16_t acked = uint16_t(p[0] << 8 | p[1]);
    // 0xFFFF: the firmware could not even find the start byte; it concerns the frame in flight.
    if (acked != out_.front().type && acked != 0xFFFF) return;
    if (p[2] != 0) LOGW("zigbee: radio rejected frame 0x%04x: status 0x%02x", out_.front().type, p[2]);
    finish_head(p[2]);
    return;
  }
  if (type == kBootInd) {
    if (reset_held_) return;  // queued before the pulse; the real one follows its release
    LOGI("zigbee: radio application started");
    if (!resetting_) {
      // Spontaneous restart (watchdog or crash): what is in flight never gets its ACK.
      for (uint64_t *t : {&ack_timer_, &hold_timer_}) {
        if (*t) loop_.cancel_timer(*t);
        *t = 0;
      }
      in_flight_ = false;
      hold_for_ = 0;
      for (auto &o : out_) o.tries = 0;
    }
    booted();
    return;
  }
  const bool releases = hold_for_ && type == hold_for_;
  if (frame_cb_) frame_cb_(type, p, n);
  if (releases) release_hold();
}

}  // namespace cs
