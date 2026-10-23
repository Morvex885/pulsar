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
//
// Case colour: the stock firmware corrects the LED colour for the case fabric with a 3x3 matrix
// per colour (/system/vendor/quasar/ledconfigs/<colour>.json, chosen by the Yandex backend's
// device_color, which is not stored on the device).
#include <dirent.h>
#include <fcntl.h>
#include <linux/fb.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <algorithm>
#include <cmath>
#include <cstdlib>
#include <cstring>
#include <vector>

#include "audio/music_frame.h"
#include "core/log.h"
#include "hw/ambient.h"
#include "hw/hw.h"
#include "hw/led_fx.h"
#include "hw/sysfs.h"

namespace cs {

using fx::Col;

namespace {

constexpr int kFrameMs = 33;          // ~30 fps while animating (stock 20 ms frames resampled by time)
constexpr float kMaxScale = 0.8f;     // global cap: never drive LEDs above 80 % PWM
constexpr int64_t kVolumeShowMs = 1500;
constexpr int64_t kFadeMs = 250;      // crossfade between layers
constexpr int64_t kMaxStepMs = 100;   // longest time step of one frame (after an idle stretch)
constexpr int64_t kCycleStyleMs = 30000, kCycleFadeMs = 1000;  // "All styles"
constexpr int64_t kWarnEveryMs = 10000;  // rate limit for repeated per-frame write warnings
constexpr const char *kLedDir = "/sys/class/leds/";
constexpr const char *kCaseDir = "/system/vendor/quasar/ledconfigs/";

enum Layer { L_VOLUME = 1, L_VOICE, L_ONESHOT, L_SHOW, L_MUTE, L_USER, L_MUSIC, L_OFF };

fx::Voice to_voice(UiState s) {
  switch (s) {
    case UiState::LISTENING: return fx::Voice::LISTENING;
    case UiState::THINKING: return fx::Voice::THINKING;
    case UiState::SPEAKING: return fx::Voice::SPEAKING;
    case UiState::ERROR: return fx::Voice::ERROR;
    case UiState::TIMER_RING:
    case UiState::IDLE:  // never drawn: voice_shown() is false
      break;
  }
  return fx::Voice::TIMER_RING;
}

Col to_col(Rgb c) { return fx::rgb(c.r, c.g, c.b); }

}  // namespace

struct Leds::Impl {
  Impl(Loop &l, const Model &m)
      : loop(l), model(m), n(m.ring.count), nlogo(m.logo.count), short_ring(!fx::wide(m.ring.count)),
        shown(size_t(n + nlogo)), from(size_t(n + nlogo)), target(size_t(n + nlogo)), alt(size_t(n)) {}
  Loop &loop;
  const Model &model;
  const int n, nlogo;     // ring / logo LED counts
  const bool short_ring;  // Lite 2: no logo, mute shown on the ring

  // output
  int fb_ring = -1, fb_logo = -1;
  fb_var_screeninfo var_ring{}, var_logo{};
  std::vector<int> sysfs_fds;  // fallback: [ring r,g,b x n][logo r,g,b x nlogo]
  std::vector<uint8_t> last;
  bool has_matrix = false;
  float matrix[3][3] = {};

  // voice state
  UiState state = UiState::IDLE;
  int64_t state_t0 = 0;
  bool voice_indication = true;
  // one-shots: the mute sweep shows over the timer-started countdown, which goes on underneath
  enum class Sweep { NONE, MUTE_ON, MUTE_OFF } sweep = Sweep::NONE;
  int64_t sweep_t0 = 0;
  bool timer_shot = false;
  int64_t timer_t0 = 0;
  bool muted = false, mute_known = false;
  // led_show
  bool show_on = false;
  std::string show_effect;
  Col show_col;
  int64_t show_until = 0;
  fx::EffectState show_fx;
  // user light
  bool user_on = false;
  Col user_col = fx::rgb(255, 255, 255);
  float user_bri = 1.f;
  std::string user_effect;
  fx::EffectState user_fx;
  bool user_changed = false;
  uint32_t user_transition = 0;
  // music
  size_t style = 0;  // index into music_styles(): 0 Off, 1 All styles, then fx::Style + 2
  bool media = false;
  MusicFrame mus_in;          // latest frame of the source in use
  bool mus_beat = false;      // beat in a frame not yet drawn
  int64_t mus_in_ms = 0, sendspin_until = 0;
  enum class Source { NONE, SPEAKER, SENDSPIN } source = Source::NONE;  // for the log
  fx::MusicInput mus_shown;
  fx::MusicState mus_st;
  int64_t cycle_t0 = 0;
  // volume overlay
  float volume = 0;
  int64_t volume_until = 0;
  // time
  float speed = 1;
  double phase = 0;  // speed-scaled seconds
  int64_t last_ms = 0;
  // composition
  int layer = 0;
  std::vector<Col> shown, from, target, alt;  // ring + logo; alt: next music style ("All styles")
  int64_t fade_t0 = 0, fade_ms = 0;

  uint64_t timer = 0, expiry_timer = 0;
  AmbientDimmer dimmer;
  int64_t warn_t[2] = {0, 0};  // last fb0/fb1 write warning
  unsigned warn_suppressed[2] = {0, 0};

  ~Impl() {
    if (timer) loop.cancel_timer(timer);
    if (expiry_timer) loop.cancel_timer(expiry_timer);
    clear();  // do not leave the last frame lit after the daemon exits
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

  // Ring/logo brightness factor from ambient light: 0.3 in the dark .. 1.
  float dim() const { return 0.3f + 0.7f * (dimmer.factor() - 0.02f) / 0.98f; }

  void clear() {
    std::vector<Col> off(size_t(n + nlogo));
    last.clear();
    output(off.data());
  }

  void warn_write(int which, const char *what) {
    int64_t now = Loop::now_ms();
    if (warn_t[which] && now - warn_t[which] < kWarnEveryMs) {
      warn_suppressed[which]++;
      return;
    }
    LOGW("leds: %s write: %s (%u similar suppressed)", what, strerror(errno), warn_suppressed[which]);
    warn_t[which] = now;
    warn_suppressed[which] = 0;
  }

  // Writes count pixels of px (RGB) to a framebuffer as BGR and latches them.
  // which: 0 = ring, 1 = logo (write warning rate limit).
  void write_fb(int fd, fb_var_screeninfo &var, const uint8_t *px, int count, int which, const char *what) {
    std::vector<uint8_t> bgr(size_t(count) * 3);
    for (int i = 0; i < count; i++) {
      bgr[i * 3] = px[i * 3 + 2];
      bgr[i * 3 + 1] = px[i * 3 + 1];
      bgr[i * 3 + 2] = px[i * 3];
    }
    if (pwrite(fd, bgr.data(), bgr.size(), 0) != ssize_t(bgr.size())) warn_write(which, what);
    ioctl(fd, FBIOPAN_DISPLAY, &var);  // latch
  }

  // Gamma 2 (perceived brightness roughly linear in the colour values), ambient dimming (on the
  // perceptual scale), case matrix (hue kept when it would clip), 80 % cap.
  void to_px(const Col &c, uint8_t *px) const {
    const float d = dim();
    float p[3] = {fx::clamp01(c.r) * d, fx::clamp01(c.g) * d, fx::clamp01(c.b) * d};
    for (float &v : p) v *= v;
    if (has_matrix) {
      float q[3];
      for (int r = 0; r < 3; r++)
        q[r] = std::max(0.f, matrix[r][0] * p[0] + matrix[r][1] * p[1] + matrix[r][2] * p[2]);
      float m = std::max({q[0], q[1], q[2]});
      for (int r = 0; r < 3; r++) p[r] = m > 1 ? q[r] / m : q[r];
    }
    for (int k = 0; k < 3; k++) px[k] = uint8_t(std::lround(std::min(1.f, p[k]) * kMaxScale * 255.f));
  }

  void output(const Col *frame) {
    std::vector<uint8_t> px(size_t(n + nlogo) * 3);
    for (int i = 0; i < n + nlogo; i++) to_px(frame[i], &px[size_t(i) * 3]);
    const bool have_last = !last.empty();
    if (fb_ring >= 0) {
      if (!have_last || memcmp(px.data(), last.data(), size_t(n) * 3) != 0)
        write_fb(fb_ring, var_ring, px.data(), n, 0, "ring fb");
      if (fb_logo >= 0 && (!have_last || memcmp(px.data() + n * 3, last.data() + n * 3, size_t(nlogo) * 3) != 0))
        write_fb(fb_logo, var_logo, px.data() + n * 3, nlogo, 1, "logo fb");
    } else {
      for (size_t k = 0; k < sysfs_fds.size(); k++)
        if (!have_last || px[k] != last[k]) hwutil::write_int_fd(sysfs_fds[k], px[k]);
    }
    last = std::move(px);
  }

  bool voice_shown() const {
    switch (state) {
      case UiState::IDLE: return false;
      case UiState::ERROR:
      case UiState::TIMER_RING: return true;
      default: return voice_indication;
    }
  }

  bool music_shown() const { return media && style != 0; }

  // Smoothed music inputs: peaks held and released over 250 ms, beat flash over 150 ms.
  void update_music(int64_t now, double dt_s) {
    const bool fresh = now - mus_in_ms < kMusicStaleMs;
    const float rel = float(std::exp(-dt_s / 0.25));
    for (size_t b = 0; b < 8; b++)
      mus_shown.bands[b] = std::max(fresh ? mus_in.bands[b] : 0.f, mus_shown.bands[b] * rel);
    mus_shown.loudness = std::max(fresh ? mus_in.loudness : 0.f, mus_shown.loudness * rel);
    mus_shown.flash *= float(std::exp(-dt_s / 0.15));
    if (mus_beat) mus_shown.flash = 1;
  }

  void render_style(fx::Style s, Col *out) const { fx::music(s, mus_st, mus_shown, phase, out, n); }

  void render_music(int64_t now, Col *out) {
    if (style >= 2) return render_style(fx::Style(style - 2), out);
    // All styles: Spectrum -> Pulse -> Disco -> Polar, 30 s each on the wall clock, 1 s crossfade.
    const int64_t t = now - cycle_t0;
    const int i = int(t / kCycleStyleMs % 4);
    render_style(fx::Style(i), out);
    const int64_t into = t % kCycleStyleMs;
    if (into < kCycleStyleMs - kCycleFadeMs) return;
    render_style(fx::Style((i + 1) % 4), alt.data());
    const float f = fx::smoothstep(0, 1, float(into - (kCycleStyleMs - kCycleFadeMs)) / kCycleFadeMs);
    for (int k = 0; k < n; k++) out[k] = fx::lerp(out[k], alt[size_t(k)], f);
  }

  // Draws the active layer into target (ring + logo); returns true if it moves.
  bool render(int64_t now, double dt, int lay) {
    Col *ring = target.data(), *logo = target.data() + n;
    std::fill(target.begin(), target.end(), Col{});
    bool animated = true;
    switch (lay) {
      case L_VOLUME:
        fx::volume(volume, ring, n);
        animated = false;
        break;
      case L_VOICE: fx::voice(to_voice(state), now - state_t0, ring, n); break;
      case L_ONESHOT:
        if (sweep != Sweep::NONE)
          fx::mute_sweep(sweep == Sweep::MUTE_ON, now - sweep_t0, ring, n);
        else
          fx::timer_started(now - timer_t0, ring, n);
        break;
      case L_SHOW: animated = fx::effect(show_effect, show_col, 1, phase, dt, show_fx, ring, n); break;
      case L_MUTE:
        std::fill(ring, ring + n, fx::mute_steady());
        animated = false;
        break;
      case L_USER: animated = fx::effect(user_effect, user_col, user_bri, phase, dt, user_fx, ring, n); break;
      case L_MUSIC: render_music(now, ring); break;
      default: animated = false; break;
    }
    if (nlogo) {
      if (voice_shown()) {
        fx::voice_logo(to_voice(state), now - state_t0, logo, nlogo);
        animated = true;
      } else if (muted) {
        std::fill(logo, logo + nlogo, fx::mute_logo());
      } else {
        Col avg;
        for (int i = 0; i < n; i++) avg = avg + ring[i] * (1.f / n);
        std::fill(logo, logo + nlogo, avg);
      }
    }
    return animated;
  }

  int active_layer(int64_t now) const {
    if (now < volume_until) return L_VOLUME;
    if (voice_shown()) return L_VOICE;
    if (sweep != Sweep::NONE || timer_shot) return L_ONESHOT;
    if (show_on) return L_SHOW;
    if (short_ring && muted) return L_MUTE;
    if (user_on && user_bri > 0) return L_USER;
    if (music_shown()) return L_MUSIC;
    return L_OFF;
  }

  // Ends timed content that ran out.
  void expire(int64_t now) {
    if (state == UiState::ERROR && now - state_t0 >= fx::kErrorMs) set_state(UiState::IDLE, now);
    if (sweep != Sweep::NONE && now - sweep_t0 >= fx::kMuteSweepMs) sweep = Sweep::NONE;
    if (timer_shot && now - timer_t0 >= fx::kTimerStartedMs) timer_shot = false;
    if (show_on && now >= show_until) show_on = false;
  }

  // Earliest pending expiry after now (0 = none).
  int64_t next_expiry(int64_t now) const {
    int64_t t = 0;
    auto take = [&](int64_t e) {
      if (e > now && (!t || e < t)) t = e;
    };
    take(volume_until);
    if (state == UiState::ERROR) take(state_t0 + fx::kErrorMs);
    if (sweep != Sweep::NONE) take(sweep_t0 + fx::kMuteSweepMs);
    if (timer_shot) take(timer_t0 + fx::kTimerStartedMs);
    if (show_on) take(show_until);
    return t;
  }

  void set_state(UiState s, int64_t now) {
    state = s;
    state_t0 = now;
  }

  void frame() {
    const int64_t now = Loop::now_ms();
    const int64_t step = last_ms ? std::clamp<int64_t>(now - last_ms, 0, kMaxStepMs) : 0;
    last_ms = now;
    const double dt = step / 1000.0 * speed;
    phase += dt;
    expire(now);

    const int lay = active_layer(now);
    if (lay == L_MUSIC) {
      update_music(now, step / 1000.0);
      fx::music_step(mus_st, mus_shown, mus_beat, phase, dt);
    }
    mus_beat = false;
    bool animated = render(now, dt, lay);

    // Fades: between layers 250 ms (the volume overlay appears at once), user light changes over
    // their transition.
    int64_t fade = -1;
    if (lay != layer) fade = lay == L_VOLUME ? 0 : kFadeMs;
    if (user_changed && (lay == L_USER || (layer == L_USER && lay != layer))) fade = user_transition;
    user_changed = false;
    layer = lay;
    if (fade >= 0) {
      from = shown;
      fade_t0 = now;
      fade_ms = fade;
    }
    if (fade_ms > 0 && now - fade_t0 < fade_ms) {
      const float f = fx::smoothstep(0, 1, float(now - fade_t0) / float(fade_ms));
      for (size_t i = 0; i < shown.size(); i++) shown[i] = fx::lerp(from[i], target[i], f);
      animated = true;
    } else {
      fade_ms = 0;
      shown = target;
    }
    output(shown.data());

    if (animated) {
      if (expiry_timer) loop.cancel_timer(expiry_timer), expiry_timer = 0;
      if (!timer) timer = loop.add_timer(kFrameMs, [this] { frame(); }, kFrameMs);
      return;
    }
    if (timer) loop.cancel_timer(timer), timer = 0;
    if (expiry_timer) loop.cancel_timer(expiry_timer), expiry_timer = 0;
    if (int64_t e = next_expiry(now))
      expiry_timer = loop.add_timer(e - now, [this] {
        expiry_timer = 0;
        frame();
      });
  }

  // Stock format: {"type": "matrix", "matrix": [[r0], [r1], [r2]]}; the key, not the type value.
  bool load_matrix(const std::string &stem) {
    std::string text;
    if (!hwutil::read_file(kCaseDir + stem + ".json", text)) return false;
    static constexpr char kKey[] = "\"matrix\"";
    size_t pos = 0;
    for (;; pos += sizeof(kKey) - 1) {
      pos = text.find(kKey, pos);
      if (pos == std::string::npos) return false;
      size_t colon = text.find_first_not_of(" \t\r\n", pos + sizeof(kKey) - 1);
      if (colon != std::string::npos && text[colon] == ':') {
        pos = text.find('[', colon);
        break;
      }
    }
    if (pos == std::string::npos) return false;
    const char *p = text.c_str() + pos;
    float m[9];
    int got = 0;
    while (*p && *p != '}' && got < 9) {
      if ((*p >= '0' && *p <= '9') || *p == '-' || *p == '.') {
        char *end = nullptr;
        double v = strtod(p, &end);
        if (end == p || !std::isfinite(v)) return false;
        m[got++] = float(v);
        p = end;
      } else {
        p++;
      }
    }
    if (got < 9) return false;
    for (int i = 0; i < 9; i++) matrix[i / 3][i % 3] = m[i];
    return true;
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
  const int64_t now = Loop::now_ms();
  if (s == d.state) return;
  // The error indication runs to its end; only another active state replaces it.
  if (d.state == UiState::ERROR && s == UiState::IDLE && now - d.state_t0 < fx::kErrorMs) return;
  d.set_state(s, now);
  d.frame();
}

void Leds::set_muted(bool muted) {
  auto &d = *impl_;
  if (d.mute_known && muted == d.muted) return;
  const bool first = !d.mute_known;
  d.mute_known = true;
  d.muted = muted;
  if (!first && (muted || !d.short_ring)) {
    d.sweep = muted ? Impl::Sweep::MUTE_ON : Impl::Sweep::MUTE_OFF;
    d.sweep_t0 = Loop::now_ms();
  } else if (!first && d.sweep == Impl::Sweep::MUTE_ON) {
    d.sweep = Impl::Sweep::NONE;  // Lite 2 unmute: no sweep, the flash goes with the mute
  }
  d.frame();
}

void Leds::set_user_light(bool on, Rgb color, float brightness, const std::string &effect, uint32_t transition_ms) {
  auto &d = *impl_;
  d.user_on = on;
  d.user_col = to_col(color);
  d.user_bri = std::max(0.f, std::min(1.f, brightness));
  d.user_effect = effect;
  d.user_changed = true;
  d.user_transition = transition_ms;
  d.frame();
}

void Leds::set_ambient_lux(float lux) {
  auto &d = *impl_;
  if (d.dimmer.update(lux)) d.frame();
}

void Leds::show_volume(float v) {
  auto &d = *impl_;
  d.volume = std::max(0.f, std::min(1.f, v));
  d.volume_until = Loop::now_ms() + kVolumeShowMs;
  d.frame();
}

void Leds::show_timer_started() {
  auto &d = *impl_;
  d.timer_shot = true;
  d.timer_t0 = Loop::now_ms();
  d.frame();
}

void Leds::show_temporary(const std::string &effect, Rgb color, int seconds) {
  auto &d = *impl_;
  d.show_on = true;
  d.show_effect = effect;
  d.show_col = to_col(color);
  if (seconds <= 0) seconds = 5;
  d.show_until = Loop::now_ms() + int64_t(std::min(seconds, 3600)) * 1000;
  d.show_fx = {};
  d.frame();
}

void Leds::clear_temporary() {
  auto &d = *impl_;
  if (!d.show_on) return;
  d.show_on = false;
  d.frame();
}

void Leds::set_voice_indication(bool on) {
  auto &d = *impl_;
  if (on == d.voice_indication) return;
  d.voice_indication = on;
  d.frame();
}

void Leds::set_speed(float k) { impl_->speed = std::isfinite(k) ? std::clamp(k, 0.25f, 4.f) : 1.f; }

void Leds::set_music_style(const std::string &style) {
  auto &d = *impl_;
  const auto &all = music_styles();
  const size_t i = size_t(std::find(all.begin(), all.end(), style) - all.begin());
  if (i == all.size() || i == d.style) return;
  d.style = i;
  d.cycle_t0 = Loop::now_ms();
  d.frame();
}

void Leds::set_media_playing(bool playing) {
  auto &d = *impl_;
  if (playing == d.media) return;
  d.media = playing;
  if (playing) {
    d.cycle_t0 = Loop::now_ms();
    d.mus_shown = {};
  } else {
    d.source = Impl::Source::NONE;
  }
  d.frame();
}

void Leds::music_frame(const MusicFrame &f, bool from_sendspin) {
  auto &d = *impl_;
  const int64_t now = Loop::now_ms();
  if (from_sendspin)
    d.sendspin_until = now + kSendspinFreshMs;
  else if (now < d.sendspin_until)
    return;
  const auto src = from_sendspin ? Impl::Source::SENDSPIN : Impl::Source::SPEAKER;
  if (src != d.source && d.media) {
    d.source = src;
    LOGI("leds: music source %s", from_sendspin ? "sendspin" : "speaker");
  }
  d.mus_in = f;
  d.mus_in_ms = now;
  if (f.beat) d.mus_beat = true;
}

bool Leds::set_case_color(const std::string &stem) {
  auto &d = *impl_;
  bool ok = true;
  if (stem.empty()) {
    d.has_matrix = false;
  } else if (d.load_matrix(stem)) {
    d.has_matrix = true;
    LOGI("leds: case colour %s", stem.c_str());
  } else {
    LOGW("leds: case colour %s unreadable", stem.c_str());
    d.has_matrix = false;
    ok = false;
  }
  d.last.clear();  // rewrite every LED with the new correction
  d.frame();
  return ok;
}

const std::vector<std::string> &Leds::effect_names() {
  static const std::vector<std::string> kNames = {"Rainbow", "Breathe", "Color loop", "Comet",  "Twinkle",
                                                  "Fire",    "Aurora",  "Lava lamp",  "Candle", "Night light"};
  return kNames;
}

// After "Off" and "All styles", in fx::Style order.
const std::vector<std::string> &Leds::music_styles() {
  static const std::vector<std::string> kStyles = {"Off", "All styles", "Spectrum", "Pulse", "Disco", "Polar"};
  return kStyles;
}

std::vector<std::string> Leds::case_colors() {
  std::vector<std::string> out;
  if (DIR *dir = opendir(kCaseDir)) {
    while (dirent *e = readdir(dir)) {
      std::string name = e->d_name;
      if (name.size() > 5 && name.compare(name.size() - 5, 5, ".json") == 0)
        out.push_back(name.substr(0, name.size() - 5));
    }
    closedir(dir);
  }
  std::sort(out.begin(), out.end());
  return out;
}

}  // namespace cs
