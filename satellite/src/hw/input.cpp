// Buttons via evdev (devices matched by name in /proc/bus/input/devices, names per Model).
// Station Midi: touch keys "awinic_sar", mute switch "mute"; Station Lite 2: touch keys "aw9610x",
// mute switch "gpio-keys"; Station Mini 2: touch keys "sar0_pad0", mute switch on a plain GPIO
// (Model::mute_gpio, polled from sysfs: its firmware has no evdev switch). The touch keys use the
// same key codes and the switch the same meaning:
//
// touch keys (capacitive, EV_KEY only; Midi KEY bitmap "40 0 0 0 0 0 0 0 c000000000000 0"):
//   KEY_VOICECOMMAND (582) -> ACTION   (maind: "ACTIVATE")
//   KEY_VOLUMEUP     (115) -> VOLUME_UP
//   KEY_VOLUMEDOWN   (114) -> VOLUME_DOWN
// mute switch (gpio-keys, KEY bitmap bit 113 KEY_MUTE, SW bitmap 0x4000 = SW_MUTE_DEVICE (14)):
//   the hardware microphone mute slider/toggle; SW_MUTE_DEVICE=1 means muted. Its KEY_MUTE
//   press/release events are ignored, the switch state is authoritative (initial value via
//   EVIOCGSW). Where the firmware has no such device (Mini 2) there is a momentary mute button
//   instead, driving one latched GPIO line: pulled up, low while muted, so the level is the state.
// ACTION: SHORT on release before 800 ms, LONG as soon as it has been held for 800 ms.
// VOLUME_*: SHORT on press, auto-repeated every 200 ms after being held for 500 ms, for at most 10 s.
// Devices that disappear (ENODEV) are looked up again every 3 s; SYN_DROPPED triggers a state resync.
#include <fcntl.h>
#include <linux/input.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>
#include <sstream>

#include "core/log.h"
#include "hw/hw.h"
#include "hw/sysfs.h"

namespace cs {

namespace {

constexpr int kKeyAction = 582;  // KEY_VOICECOMMAND
constexpr int64_t kLongPressMs = 800;
constexpr int64_t kRepeatDelayMs = 500;
constexpr int64_t kRepeatMs = 200;
constexpr int64_t kMaxHoldMs = 10000;  // a key held longer than this is taken as stuck
constexpr int64_t kReopenMs = 3000;    // retry period for missing/vanished input devices
constexpr int64_t kGpioPollMs = 100;   // GPIO mute switch sampling period
// maind's buttonsClickLimiter for "activate" (quasar.cfg): touches shorter than 56 ms and clicks
// closer than 300 ms to the previous one are dropped.
constexpr int64_t kMinActionPressMs = 56;
constexpr int64_t kMinActionGapMs = 300;

// Finds /dev/input/eventN for the device with the given name.
std::string find_event_dev(const std::string &name) {
  std::string all;
  if (!hwutil::read_file("/proc/bus/input/devices", all)) return {};
  std::istringstream in(all);
  std::string line;
  bool match = false;
  while (std::getline(in, line)) {
    if (line.rfind("N: Name=", 0) == 0) {
      match = line == "N: Name=\"" + name + "\"";
    } else if (match && line.rfind("H: Handlers=", 0) == 0) {
      std::istringstream h(line.substr(12));
      std::string tok;
      while (h >> tok)
        if (tok.rfind("event", 0) == 0) return "/dev/input/" + tok;
    } else if (line.empty()) {
      match = false;
    }
  }
  return {};
}

bool test_bit(const unsigned long *bits, int n) {
  return bits[n / (8 * sizeof(long))] >> (n % (8 * sizeof(long))) & 1;
}

}  // namespace

struct Input::Impl {
  Impl(Loop &l, const Model &m) : loop(l), model(m) {}
  Loop &loop;
  const Model &model;
  int keys_fd = -1, mute_fd = -1;
  int mute_gpio_fd = -1;  // GPIO mute switch value file
  bool muted = false;
  ButtonCb button_cb;
  MuteCb mute_cb;

  int64_t action_down = 0;  // 0 = not pressed
  int64_t last_action_ms = 0;
  bool action_long_sent = false;
  uint64_t action_timer = 0;
  uint64_t repeat_timer = 0;
  int repeat_code = 0;
  int64_t repeat_since = 0;
  uint64_t retry_timer = 0;
  uint64_t mute_poll_timer = 0;
  // After SYN_DROPPED the kernel discards events up to the next SYN_REPORT; the state is then
  // read back from the device.
  bool keys_dropped = false, mute_dropped = false;

  ~Impl() {
    for (uint64_t t : {action_timer, repeat_timer, retry_timer, mute_poll_timer})
      if (t) loop.cancel_timer(t);
    close_dev(keys_fd);
    close_dev(mute_fd);
    if (mute_gpio_fd >= 0) close(mute_gpio_fd);
  }

  void close_dev(int &fd) {
    if (fd < 0) return;
    loop.del_fd(fd);
    close(fd);
    fd = -1;
  }

  void emit(Button b, Press p) {
    LOGI("input: %s %s", b == Button::ACTION ? "action" : b == Button::VOLUME_UP ? "volume up" : "volume down",
         p == Press::LONG ? "long" : "short");
    if (button_cb) button_cb(b, p);
  }

  // Forgets every pressed key (device lost or events dropped): no release will ever arrive.
  void release_all() {
    if (action_timer) loop.cancel_timer(action_timer);
    if (repeat_timer) loop.cancel_timer(repeat_timer);
    action_timer = repeat_timer = 0;
    action_down = 0;
    repeat_code = 0;
  }

  void on_key(int code, int value) {
    if (code == kKeyAction) {
      if (value == 1 && !action_down) {
        action_down = Loop::now_ms();
        action_long_sent = false;
        action_timer = loop.add_timer(kLongPressMs, [this] {
          action_timer = 0;
          action_long_sent = true;
          emit(Button::ACTION, Press::LONG);
          // Stuck-key guard: a release that never arrives must not block later presses.
          action_timer = loop.add_timer(kMaxHoldMs - kLongPressMs, [this] {
            action_timer = 0;
            action_down = 0;
            LOGW("input: action key held > %d s, assuming released", int(kMaxHoldMs / 1000));
          });
        });
      } else if (value == 0 && action_down) {
        const int64_t now = Loop::now_ms(), held = now - action_down;
        action_down = 0;
        if (action_timer) {
          loop.cancel_timer(action_timer);
          action_timer = 0;
        }
        if (action_long_sent) return;
        // Capacitive noise (a hand near the top, flipping the mute switch) produces very short
        // touches, often in bursts; maind's "buttonsClickLimiter" drops the same.
        if (held < kMinActionPressMs || now - last_action_ms < kMinActionGapMs) {
          LOGI("input: action touch ignored (held %lld ms)", (long long)held);
          return;
        }
        last_action_ms = now;
        emit(Button::ACTION, Press::SHORT);
      }
      return;
    }
    if (code != KEY_VOLUMEUP && code != KEY_VOLUMEDOWN) {
      LOGD("input: unhandled key %d=%d", code, value);
      return;
    }
    Button b = code == KEY_VOLUMEUP ? Button::VOLUME_UP : Button::VOLUME_DOWN;
    if (value == 1) {
      emit(b, Press::SHORT);
      if (repeat_timer) loop.cancel_timer(repeat_timer);
      repeat_code = code;
      repeat_since = Loop::now_ms();
      repeat_timer = loop.add_timer(kRepeatDelayMs, [this, b] {
        if (repeat_timer) loop.cancel_timer(repeat_timer);
        repeat_timer = loop.add_timer(kRepeatMs, [this, b] {
          if (Loop::now_ms() - repeat_since > kMaxHoldMs) {
            LOGW("input: volume key held > %d s, assuming released", int(kMaxHoldMs / 1000));
            loop.cancel_timer(repeat_timer);
            repeat_timer = 0;
            repeat_code = 0;
            return;
          }
          emit(b, Press::SHORT);
        }, kRepeatMs);
        emit(b, Press::SHORT);
      });
    } else if (value == 0 && repeat_code == code) {
      if (repeat_timer) loop.cancel_timer(repeat_timer);
      repeat_timer = 0;
      repeat_code = 0;
    }
  }

  void set_muted(bool m) {
    if (m == muted) return;
    muted = m;
    LOGI("input: hardware mute %s", m ? "on" : "off");
    if (mute_cb) mute_cb(m);
  }

  void read_mute_state() {
    unsigned long sw[(SW_MAX + 1 + 8 * sizeof(long) - 1) / (8 * sizeof(long))] = {};
    if (ioctl(mute_fd, EVIOCGSW(sizeof sw), sw) >= 0) set_muted(test_bit(sw, SW_MUTE_DEVICE));
    else LOGW("input: EVIOCGSW: %s", strerror(errno));
  }

  // GPIO mute switch: exports the line (if nothing else did) and opens its value file. maind
  // leaves it exported, so after "q stop" the file is usually already there; a pin that another
  // process still owns fails the export, and we only retry later.
  int open_gpio_mute() {
    const std::string num = std::to_string(model.mute_gpio);
    const std::string dir = "/sys/class/gpio/gpio" + num;
    std::string cur;
    if (!hwutil::read_file(dir + "/direction", cur)) {
      hwutil::write_file("/sys/class/gpio/export", num);
      hwutil::write_file(dir + "/direction", "in");
      if (!hwutil::read_file(dir + "/direction", cur)) {
        LOGW("input: gpio%s not available for the mute switch", num.c_str());
        return -1;
      }
    }
    int fd = ::open((dir + "/value").c_str(), O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
      LOGW("input: open %s/value: %s", dir.c_str(), strerror(errno));
      return -1;
    }
    LOGI("input: mute switch on gpio%s (%s)", num.c_str(), hwutil::trim(cur).c_str());
    return fd;
  }

  // The pin cannot be polled with poll(2) (sysfs value files are always ready), so it is sampled.
  void start_gpio_poll() {
    if (mute_poll_timer) return;
    mute_poll_timer = loop.add_timer(
        kGpioPollMs,
        [this] {
          if (mute_gpio_fd >= 0) read_gpio_mute_state();
        },
        kGpioPollMs);
  }

  // Active low: the switch grounds the pulled-up line while the microphones are muted.
  void read_gpio_mute_state() {
    char b[8];
    if (lseek(mute_gpio_fd, 0, SEEK_SET) < 0 || ::read(mute_gpio_fd, b, sizeof b) <= 0) {
      LOGW("input: cannot read the mute gpio, reopening");
      close(mute_gpio_fd);
      mute_gpio_fd = -1;
      return;
    }
    set_muted(b[0] == '0');
  }

  // Key state after lost events: synthesize the releases we missed (presses are not invented).
  void resync_keys() {
    unsigned long keys[KEY_MAX / (8 * sizeof(long)) + 1] = {};
    if (ioctl(keys_fd, EVIOCGKEY(sizeof keys), keys) < 0) {
      LOGW("input: EVIOCGKEY: %s", strerror(errno));
      release_all();
      return;
    }
    if (action_down && !test_bit(keys, kKeyAction)) on_key(kKeyAction, 0);
    if (repeat_code && !test_bit(keys, repeat_code)) on_key(repeat_code, 0);
  }

  void drain(int fd) {
    input_event ev[16];
    for (;;) {
      ssize_t n = read(fd, ev, sizeof ev);
      if (n < 0) {
        if (errno == EAGAIN || errno == EINTR) return;
        LOGE("input: read: %s", strerror(errno));
        if (fd == keys_fd) {
          release_all();
          close_dev(keys_fd);
        } else if (fd == mute_fd) {
          close_dev(mute_fd);
        }
        return;  // retry_timer reopens it
      }
      if (n == 0) return;
      for (size_t i = 0; i < size_t(n) / sizeof(input_event); i++) {
        const auto &e = ev[i];
        bool is_keys = fd == keys_fd;
        bool &dropped = is_keys ? keys_dropped : mute_dropped;
        if (e.type == EV_SYN) {
          if (e.code == SYN_DROPPED) {
            LOGW("input: events dropped on fd %d, resyncing", fd);
            dropped = true;
          } else if (e.code == SYN_REPORT && dropped) {
            dropped = false;
            if (is_keys) resync_keys();
            else read_mute_state();
          }
          continue;
        }
        if (dropped) continue;
        if (e.type == EV_KEY && is_keys) on_key(e.code, e.value);
        else if (e.type == EV_SW && e.code == SW_MUTE_DEVICE) set_muted(e.value != 0);
      }
    }
  }

  int open_dev(const std::string &name, bool quiet) {
    std::string path = find_event_dev(name);
    if (path.empty()) {
      if (!quiet) LOGE("input: device \"%s\" not found", name.c_str());
      return -1;
    }
    int fd = ::open(path.c_str(), O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0) {
      if (!quiet) LOGE("input: open %s: %s", path.c_str(), strerror(errno));
      return -1;
    }
    LOGI("input: \"%s\" -> %s", name.c_str(), path.c_str());
    loop.add_fd(fd, EPOLLIN, [this, fd](uint32_t) { drain(fd); });
    return fd;
  }

  // Devices that were missing at start or vanished (ENODEV) are looked up again.
  void reopen_missing() {
    if (keys_fd < 0) {
      keys_fd = open_dev(model.keys_input, true);
      keys_dropped = false;
    }
    if (mute_fd < 0 && !model.mute_input.empty()) {
      mute_fd = open_dev(model.mute_input, true);
      mute_dropped = false;
      if (mute_fd >= 0) read_mute_state();
    }
    if (mute_gpio_fd < 0 && model.mute_gpio) {
      mute_gpio_fd = open_gpio_mute();
      if (mute_gpio_fd >= 0) {
        read_gpio_mute_state();
        start_gpio_poll();
      }
    }
  }
};

Input::Input(Loop &loop, const Model &model) : impl_(new Impl(loop, model)) {}
Input::~Input() = default;

bool Input::open() {
  auto &d = *impl_;
  d.keys_fd = d.open_dev(d.model.keys_input, false);
  if (!d.model.mute_input.empty()) {
    d.mute_fd = d.open_dev(d.model.mute_input, false);
    if (d.mute_fd >= 0) d.read_mute_state();
  } else if (d.model.mute_gpio) {
    d.mute_gpio_fd = d.open_gpio_mute();
    if (d.mute_gpio_fd >= 0) {
      d.read_gpio_mute_state();
      d.start_gpio_poll();
    }
  }
  if (d.mute_fd >= 0 || d.mute_gpio_fd >= 0) LOGI("input: initial hardware mute %s", d.muted ? "on" : "off");
  d.retry_timer = d.loop.add_timer(kReopenMs, [&d] { d.reopen_missing(); }, kReopenMs);
  return d.keys_fd >= 0 || d.mute_fd >= 0 || d.mute_gpio_fd >= 0;
}

bool Input::hw_muted() const { return impl_->muted; }
void Input::on_button(ButtonCb cb) { impl_->button_cb = std::move(cb); }
void Input::on_mute(MuteCb cb) { impl_->mute_cb = std::move(cb); }

}  // namespace cs
