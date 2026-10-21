// Buttons via evdev (devices matched by name in /proc/bus/input/devices, names per Model).
// Station Midi: touch keys "awinic_sar", mute switch "mute"; Station Lite 2: touch keys "aw9610x",
// mute switch "gpio-keys". Both use the same key codes and switch:
//
// touch keys (capacitive, EV_KEY only; Midi KEY bitmap "40 0 0 0 0 0 0 0 c000000000000 0"):
//   KEY_VOICECOMMAND (582) -> ACTION   (maind: "ACTIVATE")
//   KEY_VOLUMEUP     (115) -> VOLUME_UP
//   KEY_VOLUMEDOWN   (114) -> VOLUME_DOWN
// mute switch (gpio-keys, KEY bitmap bit 113 KEY_MUTE, SW bitmap 0x4000 = SW_MUTE_DEVICE (14)):
//   the hardware microphone mute slider/toggle; SW_MUTE_DEVICE=1 means muted. Its KEY_MUTE
//   press/release events are ignored, the switch state is authoritative (initial value via
//   EVIOCGSW).
// ACTION: SHORT on release before 800 ms, LONG as soon as it has been held for 800 ms.
// VOLUME_*: SHORT on press, auto-repeated every 200 ms after being held for 500 ms.
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
  explicit Impl(Loop &l) : loop(l) {}
  Loop &loop;
  int keys_fd = -1, mute_fd = -1;
  bool muted = false;
  ButtonCb button_cb;
  MuteCb mute_cb;

  int64_t action_down = 0;  // 0 = not pressed
  int64_t last_action_ms = 0;
  bool action_long_sent = false;
  uint64_t action_timer = 0;
  uint64_t repeat_timer = 0;
  int repeat_code = 0;

  ~Impl() {
    for (uint64_t t : {action_timer, repeat_timer})
      if (t) loop.cancel_timer(t);
    for (int fd : {keys_fd, mute_fd})
      if (fd >= 0) {
        loop.del_fd(fd);
        close(fd);
      }
  }

  void emit(Button b, Press p) {
    LOGI("input: %s %s", b == Button::ACTION ? "action" : b == Button::VOLUME_UP ? "volume up" : "volume down",
         p == Press::LONG ? "long" : "short");
    if (button_cb) button_cb(b, p);
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
      repeat_timer = loop.add_timer(kRepeatDelayMs, [this, b] {
        if (repeat_timer) loop.cancel_timer(repeat_timer);
        repeat_timer = loop.add_timer(kRepeatMs, [this, b] { emit(b, Press::SHORT); }, kRepeatMs);
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

  void drain(int fd) {
    input_event ev[16];
    for (;;) {
      ssize_t n = read(fd, ev, sizeof ev);
      if (n < 0) {
        if (errno != EAGAIN && errno != EINTR) LOGE("input: read: %s", strerror(errno));
        if (errno == ENODEV) {
          loop.del_fd(fd);
        }
        return;
      }
      if (n == 0) return;
      for (size_t i = 0; i < size_t(n) / sizeof(input_event); i++) {
        const auto &e = ev[i];
        if (e.type == EV_KEY && fd == keys_fd) on_key(e.code, e.value);
        else if (e.type == EV_SW && e.code == SW_MUTE_DEVICE) set_muted(e.value != 0);
      }
    }
  }

  int open_dev(const std::string &name) {
    std::string path = find_event_dev(name);
    if (path.empty()) {
      LOGE("input: device \"%s\" not found", name.c_str());
      return -1;
    }
    int fd = ::open(path.c_str(), O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0) {
      LOGE("input: open %s: %s", path.c_str(), strerror(errno));
      return -1;
    }
    LOGI("input: \"%s\" -> %s", name.c_str(), path.c_str());
    loop.add_fd(fd, EPOLLIN, [this, fd](uint32_t) { drain(fd); });
    return fd;
  }
};

Input::Input(Loop &loop, const Model &model) : impl_(new Impl(loop)), model_(model) {}
Input::~Input() = default;

bool Input::open() {
  auto &d = *impl_;
  d.keys_fd = d.open_dev(model_.keys_input);
  d.mute_fd = d.open_dev(model_.mute_input);
  if (d.mute_fd >= 0) {
    unsigned long sw[(SW_MAX + 1 + 8 * sizeof(long) - 1) / (8 * sizeof(long))] = {};
    if (ioctl(d.mute_fd, EVIOCGSW(sizeof sw), sw) >= 0) {
      d.muted = test_bit(sw, SW_MUTE_DEVICE);
      LOGI("input: initial hardware mute %s", d.muted ? "on" : "off");
    } else {
      LOGW("input: EVIOCGSW: %s", strerror(errno));
    }
  }
  return d.keys_fd >= 0 || d.mute_fd >= 0;
}

bool Input::hw_muted() const { return impl_->muted; }
void Input::on_button(ButtonCb cb) { impl_->button_cb = std::move(cb); }
void Input::on_mute(MuteCb cb) { impl_->mute_cb = std::move(cb); }

}  // namespace cs
