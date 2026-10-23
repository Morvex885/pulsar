#include "hw/bluetooth.h"

#include <chrono>
#include <unistd.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>

#include "core/log.h"

namespace cs {

namespace {

constexpr const char *kOn =
    "bluetoothctl power on; bluetoothctl discoverable-timeout 180; bluetoothctl pairable on; "
    "bluetoothctl discoverable on";
constexpr const char *kOff =
    "bluetoothctl discoverable off; bluetoothctl pairable off; "
    "for m in $(bluetoothctl devices Connected | cut -d' ' -f2); do bluetoothctl disconnect $m; done";
// bluetoothd resets the class on its own now and then (main.conf: loudspeaker), so it is set again
// with every window; paired devices are trusted so that they reconnect without a new pairing.
constexpr const char *kTrust = "for m in $(bluetoothctl devices Paired | cut -d' ' -f2); do bluetoothctl trust $m; done";
constexpr const char *kClassHeadphones = "hciconfig hci0 class 0x240404";  // Audio/Video, wearable headset (as WH-1000XM4)
constexpr const char *kClassSpeaker = "hciconfig hci0 class 0x4c0414";     // Audio/Video, loudspeaker
// bluealsa starts as an A2DP sink + phone (HFP-AG). Windows lists a device with the connect /
// disconnect button only when it also offers the headset roles, so they are added (once per boot).
constexpr const char *kProfiles =
    "pid=$(pidof bluealsa) || exit 0; "
    "grep -q hfp-hf /proc/$pid/cmdline && exit 0; "
    "/etc/init.d/S53bluealsa-aplay stop; /etc/init.d/S52bluealsa stop; sleep 1; "
    "start-stop-daemon -S -b -x /usr/bin/bluealsa -n bluealsa -- -p a2dp-sink -p a2dp-source -p hfp-ag "
    "-p hfp-hf -p hsp-hs --sbc-quality=high; sleep 2; /etc/init.d/S53bluealsa-aplay start";
constexpr const char *kExpired = "bluetoothctl pairable off";  // window timed out: no more new pairings

}  // namespace

Bluetooth::Bluetooth(Loop &loop, std::function<void()> changed) : loop_(loop), changed_(std::move(changed)) {}

Bluetooth::~Bluetooth() {
  *alive_ = false;
  {
    std::lock_guard<std::mutex> lk(mu_);
    stop_ = true;
  }
  cv_.notify_all();
  if (th_.joinable()) th_.join();
}

void Bluetooth::set_headphones(bool on) {
  headphones_ = on;
  {
    std::lock_guard<std::mutex> lk(mu_);
    if (want_ < 0) want_ = 3;  // a pending on / off sets the class itself
  }
  cv_.notify_all();
}

void Bluetooth::start() { th_ = std::thread([this] { run(); }); }

void Bluetooth::set_pairing(bool on) {
  {
    std::lock_guard<std::mutex> lk(mu_);
    want_ = on ? 1 : 0;
  }
  cv_.notify_all();
}

bool Bluetooth::pairing() const {
  std::lock_guard<std::mutex> lk(mu_);
  return pairing_;
}

bool Bluetooth::query_discoverable() {
  FILE *p = popen("bluetoothctl show 2>/dev/null", "r");
  if (!p) return false;
  char line[256];
  bool on = false;
  while (fgets(line, sizeof line, p))
    if (strstr(line, "Discoverable: yes")) on = true;
  pclose(p);
  return on;
}

void Bluetooth::run() {
  if (system(kProfiles) != 0) LOGW("bluetooth: could not add the headset profiles");
  // Polls instead of waiting on a std::condition_variable: libc++'s wait() returns at once on the
  // Station 2 (glibc 2.30 userland), which kept this thread at 100 % of a core.
  int idle_ticks = 0;
  for (;;) {
    int want = -1;
    bool open = false;
    {
      std::lock_guard<std::mutex> lk(mu_);
      if (stop_) return;
      want = want_;
      want_ = -1;
      open = pairing_;
    }
    if (want < 0) {
      usleep(100000);
      // while the window is open, look every 10 s whether it timed out
      if (!open || ++idle_ticks < 100) continue;
    }
    idle_ticks = 0;
    const char *cls = headphones_ ? kClassHeadphones : kClassSpeaker;
    if (want == 3) {
      system(cls);
    } else if (want == 1) {
      system(cls);
      if (system(kOn) != 0) LOGW("bluetooth: bluetoothctl failed");
    } else if (want == 0) {
      if (system(kOff) != 0) LOGW("bluetooth: bluetoothctl failed");
    }
    const bool now = query_discoverable();
    if (now) {
      system(cls);
      system(kTrust);
    }
    bool was;
    {
      std::lock_guard<std::mutex> lk(mu_);
      was = pairing_;
      pairing_ = now;
    }
    if (was && !now && want != 0) system(kExpired);
    if (was != now) {
      LOGI("bluetooth: pairing %s", now ? "open" : "closed");
      auto alive = alive_;
      loop_.post([this, alive] {
        if (*alive && changed_) changed_();
      });
    }
  }
}

}  // namespace cs
