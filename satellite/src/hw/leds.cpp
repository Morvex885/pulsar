// LED ring (+ logo) of the Station Midi (24 + 6 RGB) and Lite 2 (3 RGB, no logo); layout per Model.
//
// How the hardware is driven: the LEDs sit behind kernel LED triggers that expose them as
// framebuffers (Midi: "ledfb-ring"/"ledfb-logo" -> /dev/fb0 "ring" 24x1, /dev/fb1 "logo" 6x1;
// Lite 2: "led_ring_fb" -> /dev/fb0 "YNDX LED ring" 3x1), 24 bpp, rgba 8/16,8/8,8/0 -> bytes per
// pixel are B,G,R; pixel i = the i-th sysfs LED of the strip.
// On the Midi a pwrite() is only shown on the next write or FBIOPAN_DISPLAY (the ledfb driver
// pushes the framebuffer at the *start* of each write/pan), so every frame is pwrite() +
// FBIOPAN_DISPLAY. Cost ~2.5 ms per ring update (i2c), far cheaper than per-LED sysfs writes.
// If the framebuffers are missing or the triggers are not active, we fall back to the
// per-channel sysfs brightness files kept open.
//
// Ring index order: Yandex patterns (ledpatterns/ring/*.led, one RRGGBB column per LED) anchor
// their animations at the first LED and grow symmetrically to its neighbours (mute.led,
// alice_thinking.led), so LED 0 is the "home" position; all animations here are relative to it
// and go towards increasing indices.
#include <fcntl.h>
#include <linux/fb.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <cmath>
#include <cstring>
#include <vector>

#include "core/log.h"
#include "hw/hw.h"
#include "hw/sysfs.h"

namespace cs {

namespace {

constexpr float kCometLen24 = 7.f;    // thinking comet length on the 24-LED ring
constexpr int kFrameMs = 33;          // ~30 fps while animating
constexpr float kMaxScale = 0.8f;     // global cap: never drive LEDs above 80 % PWM
constexpr int64_t kVolumeShowMs = 1500;
constexpr const char *kLedDir = "/sys/class/leds/";

struct Col {
  float r = 0, g = 0, b = 0;
};

Col operator*(Col c, float k) { return {c.r * k, c.g * k, c.b * k}; }
Col rgb(int r, int g, int b) { return {r / 255.f, g / 255.f, b / 255.f}; }

Col hsv(float h, float s, float v) {
  h = std::fmod(h, 1.f);
  if (h < 0) h += 1.f;
  float i = std::floor(h * 6), f = h * 6 - i, p = v * (1 - s), q = v * (1 - f * s), t = v * (1 - (1 - f) * s);
  switch (int(i) % 6) {
    case 0: return {v, t, p};
    case 1: return {q, v, p};
    case 2: return {p, v, t};
    case 3: return {p, q, v};
    case 4: return {t, p, v};
    default: return {v, p, q};
  }
}

// Smooth 0..1 oscillation with period ms, starting at minimum.
float wave(int64_t t, int64_t period) { return 0.5f - 0.5f * std::cos(float(t % period) / float(period) * 6.2831853f); }

uint8_t to8(float v) {
  v = std::max(0.f, std::min(1.f, v)) * kMaxScale;
  // Gamma 2.0 so that perceived brightness is roughly linear in v.
  return uint8_t(std::lround(v * v * 255.f));
}

// Palette
const Col kCyan = rgb(0, 190, 255);
const Col kBlue = rgb(55, 15, 255);  // Yandex "alice" violet-blue (0x370fff)
const Col kWhite = rgb(255, 255, 255);
const Col kRed = rgb(255, 0, 0);
const Col kOrange = rgb(255, 90, 0);

const Col kWake = rgb(120, 230, 255);
}  // namespace

struct Leds::Impl {
  Impl(Loop &l, const Model &m) : loop(l), model(m), n(m.ring.count), nlogo(m.logo.count) {}
  Loop &loop;
  const Model &model;
  const int n, nlogo;  // ring / logo LED counts

  // output
  int fb_ring = -1, fb_logo = -1;
  fb_var_screeninfo var_ring{}, var_logo{};
  std::vector<int> sysfs_fds;  // fallback: [ring r,g,b x n][logo r,g,b x nlogo]
  std::vector<uint8_t> last;

  // state
  UiState state = UiState::IDLE;
  int64_t state_t0 = 0;
  bool user_on = false;
  Col user_col = rgb(255, 255, 255);
  float user_bri = 1.f;
  std::string effect;
  float volume = 0;
  int64_t volume_until = 0;
  uint64_t timer = 0;

  ~Impl() {
    if (timer) loop.cancel_timer(timer);
    if (fb_ring >= 0) close(fb_ring);
    if (fb_logo >= 0) close(fb_logo);
    for (int fd : sysfs_fds)
      if (fd >= 0) close(fd);
  }

  static bool trigger_is(const std::string &led, const std::string &trig) {
    std::string t;
    if (!hwutil::read_file(std::string(kLedDir) + led + "/trigger", t)) return false;
    return t.find("[" + trig + "]") != std::string::npos;
  }

  std::string led_name(const Model::LedStrip &s, const char *color, int i) const {
    return model.led_prefix + ":" + color + ":" + s.sysfs_kind + std::to_string(s.first_index + i);
  }

  // Opens one strip's framebuffer if its trigger is active and the fb has the expected name.
  int open_strip_fb(const Model::LedStrip &s, fb_var_screeninfo &var) {
    if (!trigger_is(led_name(s, "red", 0), s.trigger)) {
      LOGW("leds: trigger %s not active on %s", s.trigger.c_str(), led_name(s, "red", 0).c_str());
      return -1;
    }
    std::string name;
    hwutil::read_file("/sys/class/graphics/" + s.fb_dev.substr(s.fb_dev.rfind('/') + 1) + "/name", name);
    if (hwutil::trim(name) != s.fb_name) {
      LOGW("leds: %s is '%s', expected '%s'", s.fb_dev.c_str(), hwutil::trim(name).c_str(), s.fb_name.c_str());
      return -1;
    }
    int fd = ::open(s.fb_dev.c_str(), O_RDWR | O_CLOEXEC);
    if (fd < 0) {
      LOGW("leds: open %s: %s", s.fb_dev.c_str(), strerror(errno));
      return -1;
    }
    ioctl(fd, FBIOGET_VSCREENINFO, &var);
    return fd;
  }

  bool open_fb() {
    fb_ring = open_strip_fb(model.ring, var_ring);
    if (nlogo) fb_logo = open_strip_fb(model.logo, var_logo);
    if (fb_ring >= 0 && (!nlogo || fb_logo >= 0)) return true;
    if (fb_ring >= 0) close(fb_ring);
    if (fb_logo >= 0) close(fb_logo);
    fb_ring = fb_logo = -1;
    return false;
  }

  bool open_sysfs() {
    static const char *cols[] = {"red", "green", "blue"};
    for (int i = 0; i < n + nlogo; i++)
      for (const char *c : cols) {
        std::string path = kLedDir + (i < n ? led_name(model.ring, c, i) : led_name(model.logo, c, i - n)) +
                           "/brightness";
        int fd = ::open(path.c_str(), O_WRONLY | O_CLOEXEC);
        if (fd < 0) {
          LOGE("leds: cannot open %s", path.c_str());
          return false;
        }
        sysfs_fds.push_back(fd);
      }
    return true;
  }

  // Writes count pixels of px (RGB) to a framebuffer as BGR and latches them.
  void write_fb(int fd, fb_var_screeninfo &var, const uint8_t *px, int count, const char *what) {
    std::vector<uint8_t> bgr(size_t(count) * 3);
    for (int i = 0; i < count; i++) {
      bgr[i * 3] = px[i * 3 + 2];
      bgr[i * 3 + 1] = px[i * 3 + 1];
      bgr[i * 3 + 2] = px[i * 3];
    }
    if (pwrite(fd, bgr.data(), bgr.size(), 0) != ssize_t(bgr.size())) LOGW("leds: %s write: %s", what, strerror(errno));
    ioctl(fd, FBIOPAN_DISPLAY, &var);  // latch
  }

  void output(const Col *ring, const Col *logo) {
    std::vector<uint8_t> px(size_t(n + nlogo) * 3);
    for (int i = 0; i < n + nlogo; i++) {
      const Col &c = i < n ? ring[i] : logo[i - n];
      px[i * 3 + 0] = to8(c.r);
      px[i * 3 + 1] = to8(c.g);
      px[i * 3 + 2] = to8(c.b);
    }
    const bool have_last = !last.empty();
    if (fb_ring >= 0) {
      if (!have_last || memcmp(px.data(), last.data(), size_t(n) * 3) != 0)
        write_fb(fb_ring, var_ring, px.data(), n, "ring fb");
      if (fb_logo >= 0 && (!have_last || memcmp(px.data() + n * 3, last.data() + n * 3, size_t(nlogo) * 3) != 0))
        write_fb(fb_logo, var_logo, px.data() + n * 3, nlogo, "logo fb");
    } else {
      for (size_t k = 0; k < sysfs_fds.size(); k++)
        if (!have_last || px[k] != last[k]) hwutil::write_int_fd(sysfs_fds[k], px[k]);
    }
    last = std::move(px);
  }

  // Renders the current frame; returns true if the content is animated (needs further frames).
  bool render(int64_t now, Col *ring) {
    int64_t t = now - state_t0;
    for (int i = 0; i < n; i++) ring[i] = {};
    bool animated = true;
    switch (state) {
      case UiState::IDLE:
        animated = render_user(now, ring);
        break;
      case UiState::WAKE: {
        // Sweep from ring0 outward in both directions in 250 ms, then hold bright ice-blue.
        float reach = std::min(1.f, float(t) / 250.f) * (n / 2 + 1);
        for (int i = 0; i <= n / 2; i++) {
          float k = std::max(0.f, std::min(1.f, reach - i));
          ring[i] = kWake * k;
          ring[(n - i) % n] = ring[i];
        }
        animated = t < 300;
        break;
      }
      case UiState::LISTENING: {
        float k = 0.55f + 0.45f * wave(t, 2000);
        for (int i = 0; i < n; i++) ring[i] = kCyan * k;
        break;
      }
      case UiState::THINKING: {
        // Two comets rotating (one turn per 1.2 s).
        const float len = std::max(1.5f, kCometLen24 * n / 24.f);
        float head = float(t % 1200) / 1200.f * n;
        for (int i = 0; i < n; i++) {
          for (int c = 0; c < 2; c++) {
            float d = std::fmod(head + c * (n / 2.f) - i + n * 2, float(n));  // distance behind head
            if (d < len) {
              Col v = kBlue * (1.f - d / len);
              ring[i] = {std::max(ring[i].r, v.r), std::max(ring[i].g, v.g), std::max(ring[i].b, v.b)};
            }
          }
          ring[i] = {std::max(ring[i].r, kBlue.r * 0.12f), std::max(ring[i].g, kBlue.g * 0.12f),
                     std::max(ring[i].b, kBlue.b * 0.12f)};
        }
        break;
      }
      case UiState::SPEAKING: {
        float k = 0.45f + 0.55f * wave(t, 700);
        for (int i = 0; i < n; i++) ring[i] = kBlue * k;
        break;
      }
      case UiState::ERROR: {
        // 3 blinks (200 ms on / 200 ms off), then back to IDLE.
        if (t >= 1200) {
          set_state(UiState::IDLE, now);
          return render(now, ring);
        }
        float k = (t % 400) < 200 ? 0.9f : 0.f;
        for (int i = 0; i < n; i++) ring[i] = kRed * k;
        break;
      }
      case UiState::MUTED:
        for (int i = 0; i < n; i++) ring[i] = kRed * 0.25f;
        animated = false;
        break;
      case UiState::TIMER_RING: {
        float k = 0.15f + 0.85f * wave(t, 1000);
        for (int i = 0; i < n; i++) ring[i] = kOrange * k;
        break;
      }
    }
    if (now < volume_until) {
      // Volume arc overlay: v*n LEDs from LED 0, partial last LED, remaining ring dark.
      float lit = volume * n;
      for (int i = 0; i < n; i++) {
        float k = std::max(0.f, std::min(1.f, lit - i));
        ring[i] = kWhite * (0.15f + 0.65f * k) * (k > 0 ? 1.f : 0.f);
      }
      animated = true;  // need a frame when the overlay expires
    }
    return animated;
  }

  bool render_user(int64_t now, Col *ring) {
    if (!user_on || user_bri <= 0) return false;
    Col base = user_col * user_bri;
    if (effect == "Rainbow") {
      float off = float(now % 6000) / 6000.f;
      for (int i = 0; i < n; i++) ring[i] = hsv(off + float(i) / n, 1.f, user_bri);
      return true;
    }
    if (effect == "Breathe") {
      float k = 0.1f + 0.9f * wave(now, 4000);
      for (int i = 0; i < n; i++) ring[i] = base * k;
      return true;
    }
    for (int i = 0; i < n; i++) ring[i] = base;
    return false;
  }

  void set_state(UiState s, int64_t now) {
    state = s;
    state_t0 = now;
  }

  void frame() {
    int64_t now = Loop::now_ms();
    std::vector<Col> ring(static_cast<size_t>(n));
    bool animated = render(now, ring.data());
    // Logo mirrors the ring's average colour.
    Col avg;
    for (auto &c : ring) avg = {avg.r + c.r / n, avg.g + c.g / n, avg.b + c.b / n};
    std::vector<Col> logo(size_t(nlogo), avg);
    output(ring.data(), logo.data());
    if (animated && !timer) {
      timer = loop.add_timer(kFrameMs, [this] { frame(); }, kFrameMs);
    } else if (!animated && timer) {
      loop.cancel_timer(timer);
      timer = 0;
    }
  }
};

Leds::Leds(Loop &loop, const Model &model) : impl_(new Impl(loop, model)) {}
Leds::~Leds() = default;

bool Leds::open() {
  auto &d = *impl_;
  if (d.open_fb()) {
    LOGI("leds: driving %d ring + %d logo LEDs via framebuffer", d.n, d.nlogo);
  } else if (d.open_sysfs()) {
    LOGI("leds: driving %d ring + %d logo LEDs via sysfs brightness", d.n, d.nlogo);
  } else {
    return false;
  }
  d.state_t0 = Loop::now_ms();
  d.frame();
  return true;
}

void Leds::set_ui_state(UiState s) {
  auto &d = *impl_;
  if (s == d.state) return;
  d.set_state(s, Loop::now_ms());
  d.frame();
}

void Leds::set_user_light(bool on, Rgb color, float brightness, const std::string &effect) {
  auto &d = *impl_;
  d.user_on = on;
  d.user_col = rgb(color.r, color.g, color.b);
  d.user_bri = std::max(0.f, std::min(1.f, brightness));
  d.effect = effect;
  d.frame();
}

void Leds::show_volume(float v) {
  auto &d = *impl_;
  d.volume = std::max(0.f, std::min(1.f, v));
  d.volume_until = Loop::now_ms() + kVolumeShowMs;
  d.frame();
}

}  // namespace cs
