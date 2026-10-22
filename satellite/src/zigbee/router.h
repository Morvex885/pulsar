// The Midi's radio as a Zigbee router in somebody else's network (Zigbee2MQTT, ZHA, ...): it runs
// pulsar's router firmware (radio/router), which joins and leaves on command and otherwise forwards
// on its own. Over zbhci: 0x0045 -> 0x8045 network info, 0x0002 network steering (join), 0x0005
// leave + factory new; unsolicited 0x8906 joined, 0x8920 [event][status] (1 started: status 0 = on
// a network; 2 join failed: BDB status; 3 left).
#pragma once

#include <cstdint>
#include <functional>
#include <memory>
#include <string>

#include "core/loop.h"
#include "zigbee/telink_link.h"

namespace cs {

class ZigbeeRouter {
 public:
  struct State {
    bool ready = false;    // the radio answered with its network state
    bool joined = false;
    bool joining = false;  // network steering in progress
    uint16_t pan = 0, nwk = 0;
    uint8_t channel = 0;
    std::string error;     // last failure, "" = none
  };
  struct Hooks {
    std::function<void()> changed;
  };

  // link: open, the running firmware identified as pulsar's router.
  ZigbeeRouter(Loop &loop, TelinkLink &link);
  ~ZigbeeRouter();
  void start(Hooks hooks);
  const State &state() const { return st_; }
  void join();   // the network must be open for joining (permit join on its coordinator)
  void leave();  // announces the leave and forgets the network

 private:
  void on_boot();
  void on_frame(uint16_t type, const uint8_t *p, size_t n);
  void changed();

  Loop &loop_;
  TelinkLink &link_;
  Hooks hooks_;
  State st_;
  uint64_t join_timer_ = 0;
  std::shared_ptr<int> alive_ = std::make_shared<int>(0);  // for one-shot timers outliving it
};

}  // namespace cs
