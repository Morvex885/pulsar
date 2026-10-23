// Silicon Labs EZSP NCP (Station 2's Zigbee radio) as a plain Zigbee router: pulsar is the NCP's
// host, joins the network of Zigbee2MQTT / ZHA (the NCP keeps it in its own flash, so later starts
// resume it) and the stack then repeats frames for the other devices. A worker thread owns the UART
// (ASH framing + EZSP v8 commands); the loop thread only posts join / leave and reads the status.
#pragma once

#include <atomic>
#include <condition_variable>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <thread>

#include "core/loop.h"

namespace cs {

class EzspRouter {
 public:
  // changed: called on the loop thread when status() changes.
  EzspRouter(Loop &loop, std::string tty, unsigned baud, std::function<void()> changed);
  ~EzspRouter();
  EzspRouter(const EzspRouter &) = delete;
  EzspRouter &operator=(const EzspRouter &) = delete;

  void start();
  // Scans for a network that allows joining (permit join open in Zigbee2MQTT / ZHA) and joins it as
  // a router; leave() removes the radio from its network.
  void join() { request(Cmd::JOIN); }
  void leave() { request(Cmd::LEAVE); }
  std::string status() const;

 private:
  enum class Cmd { NONE, JOIN, LEAVE };
  struct Impl;
  void request(Cmd c);

  Loop &loop_;
  std::string tty_;
  unsigned baud_;
  std::function<void()> changed_;
  std::shared_ptr<std::atomic<bool>> alive_ = std::make_shared<std::atomic<bool>>(true);
  std::atomic<bool> stop_{false};
  std::atomic<int> cmd_{0};
  mutable std::mutex mu_;
  std::string status_ = "starting";
  std::thread thread_;

  void run();
  void set_status(const std::string &s);
  friend struct Impl;
};

}  // namespace cs
