// Bluetooth speaker pairing on the Station 2: bluetoothd, bluealsa (A2DP sink) and the just-works
// agent keep running next to pulsar, so playing a phone's audio only needs the adapter to be
// discoverable and pairable for a while. A worker thread runs the bluetoothctl commands.
#pragma once

#include <atomic>
#include <condition_variable>
#include <functional>
#include <memory>
#include <mutex>
#include <thread>

#include "core/loop.h"

namespace cs {

class Bluetooth {
 public:
  // changed: loop thread, when the pairing state changes (also when the window times out).
  Bluetooth(Loop &loop, std::function<void()> changed);
  ~Bluetooth();
  Bluetooth(const Bluetooth &) = delete;
  Bluetooth &operator=(const Bluetooth &) = delete;

  void start();
  // on: discoverable + pairable for 3 minutes; off: closes the window and disconnects the phones.
  void set_pairing(bool on);
  bool pairing() const;
  // Device class shown to phones / PCs: headphones (default: listed with the audio outputs) or speaker.
  void set_headphones(bool on);

 private:
  void run();
  bool query_discoverable();

  Loop &loop_;
  std::function<void()> changed_;
  std::shared_ptr<std::atomic<bool>> alive_ = std::make_shared<std::atomic<bool>>(true);
  mutable std::mutex mu_;
  std::condition_variable cv_;
  int want_ = -1;  // -1 nothing, 0 off, 1 on, 3 only apply the device class
  std::atomic<bool> headphones_{true};
  bool stop_ = false, pairing_ = false;
  std::thread th_;
};

}  // namespace cs
