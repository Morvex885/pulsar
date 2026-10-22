// Serial link to the Station Midi's Zigbee co-processor: a Telink B91 on /dev/ttyS2 at 115200 8N1,
// no flow control, running Yandex's gateway firmware or one of pulsar's (radio/).
//
// Frames are Telink zbhci: 0x55 | type BE16 | length BE16 | XOR of type, length and payload |
// payload | 0xAA. The firmware DMA-receives exactly one frame per buffer and answers every frame
// with an ACK (type 0x8000, payload [type BE16][status][0]); a second frame sent before that ACK is
// merged into the same buffer and rejected, so frames go out stop-and-wait.
#pragma once

#include <cstdint>
#include <deque>
#include <functional>
#include <string>
#include <vector>

#include "core/loop.h"

namespace cs {

class TelinkLink {
 public:
  // Every frame from the chip except ACKs.
  using FrameCb = std::function<void(uint16_t type, const uint8_t *p, size_t n)>;
  // ACK of a sent frame: 0 = accepted, > 0 firmware status (2 = unknown type), -1 = no ACK.
  using AckCb = std::function<void(int status)>;
  // The chip (re)started its application: everything it held in RAM is gone.
  using BootCb = std::function<void()>;

  // reset_line / boot_line: GPIO value files, active low (1 = asserted).
  TelinkLink(Loop &loop, std::string tty, std::string reset_line, std::string boot_line);
  ~TelinkLink();
  TelinkLink(const TelinkLink &) = delete;
  TelinkLink &operator=(const TelinkLink &) = delete;

  void on_frame(FrameCb cb) { frame_cb_ = std::move(cb); }
  void on_boot(BootCb cb) { boot_cb_ = std::move(cb); }

  // Opens the tty and hardware-resets the chip; on_boot follows once its application runs.
  bool open();
  // hold_for: after the ACK nothing else is sent until a frame of that type arrives or hold_ms
  // pass; for commands during which the firmware busy-waits and would miss further frames.
  void send(uint16_t type, std::vector<uint8_t> payload, AckCb done = nullptr, uint16_t hold_for = 0,
            int64_t hold_ms = 0);
  // Hardware reset (reset line pulse with the boot line low). Queued frames fail with -1.
  void reset();

 private:
  struct Out {
    uint16_t type;
    std::vector<uint8_t> frame;
    AckCb done;
    uint16_t hold_for = 0;
    int64_t hold_ms = 0;
    int tries = 0;
  };
  void on_readable();
  void parse();
  void handle(uint16_t type, const uint8_t *p, size_t n);
  void pump();
  void ack_timeout();
  void finish_head(int status);
  void release_hold();
  void booted();

  Loop &loop_;
  std::string tty_;
  std::string reset_line_, boot_line_;
  int fd_ = -1;
  FrameCb frame_cb_;
  BootCb boot_cb_;
  std::vector<uint8_t> rx_;
  std::deque<Out> out_;
  bool in_flight_ = false;
  bool resetting_ = false;  // reset pulse or boot wait in progress: nothing is sent
  bool reset_held_ = false;  // reset line asserted: a boot indication now predates the pulse
  uint16_t hold_for_ = 0;   // frame type awaited before anything else is sent (0 = none)
  uint64_t ack_timer_ = 0;
  uint64_t boot_timer_ = 0;
  uint64_t hold_timer_ = 0;
  int failures_ = 0;  // consecutive frames without ACK
};

}  // namespace cs
