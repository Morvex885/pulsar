#pragma once
#include <cstdint>
#include <deque>
#include <functional>
#include <string>
#include <vector>
#include "core/loop.h"
namespace cs {
void emit(const std::string &line);
std::string hexs(const uint8_t *p, size_t n);
class TelinkLink {
 public:
  using FrameCb = std::function<void(uint16_t type, const uint8_t *p, size_t n)>;
  using AckCb = std::function<void(int status)>;
  using BootCb = std::function<void()>;
  explicit TelinkLink(Loop &loop) : loop_(loop) {}
  void on_frame(FrameCb cb) { frame_cb_ = std::move(cb); }
  void on_boot(BootCb cb) { boot_cb_ = std::move(cb); }
  void send(uint16_t type, std::vector<uint8_t> payload, AckCb done = nullptr, uint16_t hold_for = 0,
            int64_t hold_ms = 0) {
    out_.push_back(Out{type, std::move(payload), std::move(done), hold_for, hold_ms});
    pump();
  }
  struct Out { uint16_t type; std::vector<uint8_t> payload; AckCb done; uint16_t hold_for; int64_t hold_ms; };
  Loop &loop_;
  FrameCb frame_cb_;
  BootCb boot_cb_;
  std::deque<Out> out_;
  bool in_flight_ = false, read_ = false;
  uint16_t hold_ = 0;
  uint64_t hold_timer_ = 0;
  bool quiet_ = false;
  std::deque<int> acks_;
  void pump() {
    if (in_flight_ || hold_ || out_.empty()) return;
    in_flight_ = true;
    read_ = false;
  }
  void release() {
    if (hold_timer_) loop_.cancel_timer(hold_timer_);
    hold_timer_ = 0;
    hold_ = 0;
    pump();
  }
  // The radio reads the frame in flight and ACKs it.
  bool radio_step() {
    if (!in_flight_ || read_) return false;
    read_ = true;
    Out o = std::move(out_.front());
    out_.pop_front();
    if (!quiet_) {
      char b[16];
      snprintf(b, sizeof b, "%04x", o.type);
      emit(std::string("send ") + b + " " + hexs(o.payload.data(), o.payload.size()));
    }
    int st = 0;
    if (!acks_.empty()) { st = acks_.front(); acks_.pop_front(); }
    in_flight_ = false;
    if (st == 0 && o.hold_for) {
      hold_ = o.hold_for;
      hold_timer_ = loop_.add_timer(o.hold_ms, [this] { hold_timer_ = 0; hold_ = 0; pump(); });
    }
    if (o.done) o.done(st);
    pump();
    return true;
  }
  void inject(uint16_t type, std::vector<uint8_t> p) {
    const bool rel = hold_ && type == hold_;
    if (frame_cb_) frame_cb_(type, p.data(), p.size());
    if (rel) release();
  }
  void inject_boot() {
    if (hold_timer_) loop_.cancel_timer(hold_timer_);
    hold_timer_ = 0;
    in_flight_ = false;
    hold_ = 0;
    if (boot_cb_) boot_cb_();
    pump();
  }
};
}  // namespace cs
