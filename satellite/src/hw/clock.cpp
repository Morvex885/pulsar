// 4-digit 7-segment clock, one sysfs LED per segment (names per Model).
//
// Station Midi: two aw21036/aw21012 chips on i2c 3-0037 / 3-0021,
// /sys/class/leds/cucumber:white:clock-<seg><digit>.
//
// Segment mapping (derived from the live display driven by maind: at 21:17 local time the lit
// set was a1 b1 d1 e1 g1 | b2 c2 | b3 c3 | a4 b4 c4 plus p3 q3, i.e. "21:17"):
//   clock-<s><n>: n = digit position 1..4 left to right, s = standard 7-segment letter
//
//          aaa
//         f   b
//          ggg          digits: [1][2] : [3][4]
//         e   c
//          ddd
//
//   p3 + q3     colon (upper/lower dot) - the only extra LEDs maind lights for the time.
//   deg         degree sign; h1 h2, l1 l2, m1 m2, p1 p2 p4 p5, q1 q2 q4 q5: auxiliary dots/icons
//               whose position is not known (never lit by maind for the time); kept off here.
//   cucumber:sync:clock-0/1 are the chips' sync pseudo-LEDs (not segments).
// Midi brightness: like maind, segments are switched 0/255 and dimming uses the chips' global
// current attribute (/sys/bus/i2c/devices/{3-0037,3-0021}/current, 0..255; maind wrote 5 for its
// stored brightness 0.0196 = 5/255). The brightness_scale tables are left untouched.
//
// Station Lite 2: /sys/class/leds/plum:white:clock<SEG><pos>, SEG = A..G (standard segments),
// pos = 0..3 left to right, colon = DH + DL; H0..H3, DM and DEGREE stay dark (maind's
// /leds/clock frameBuilder config in quasar.cfg). No global current: maind dims through the
// per-segment brightness value (0..255), and so do we.
//
// Time is local time via localtime_r, i.e. it honours the process TZ (the device has no
// /etc/localtime or zoneinfo, so the daemon must setenv("TZ", "<POSIX TZ>") + tzset(), e.g. from
// HA's GetTimeResponse.timezone; otherwise UTC is shown).
#include <fcntl.h>
#include <unistd.h>

#include <cmath>
#include <cstring>
#include <ctime>
#include <string>
#include <vector>

#include "core/log.h"
#include "hw/ambient.h"
#include "hw/hw.h"
#include "hw/sysfs.h"

namespace cs {

namespace {

constexpr int kDigits = 4;
constexpr int kSegs = 7;  // a..g
constexpr int64_t kTickMs = 250;
constexpr int64_t kFlashMs = 1500;
constexpr int64_t kCodePageMs = 2000;
constexpr int kMinLevel = 2;  // lowest current / brightness that still lights the segments

// Bit i = segment 'a'+i.
constexpr uint8_t kFont[10] = {
    0x3f,  // 0 abcdef
    0x06,  // 1 bc
    0x5b,  // 2 abdeg
    0x4f,  // 3 abcdg
    0x66,  // 4 bcfg
    0x6d,  // 5 acdfg
    0x7d,  // 6 acdefg
    0x07,  // 7 abc
    0x7f,  // 8 abcdefg
    0x6f,  // 9 abcdfg
};
constexpr uint8_t kMinus = 0x40;  // g

// Printable ASCII 0x20..0x7E, from github.com/dmadison/LED-Segment-ASCII 7-Segment-ASCII_HEX.txt
// (MIT, Copyright (c) 2017 David Madison); decimal point bits dropped, '.' drawn like ','.
constexpr uint8_t kAscii[95] = {
    0x00, 0x06, 0x22, 0x7E, 0x6D, 0x52, 0x46, 0x20, 0x29, 0x0B, 0x21, 0x70, 0x10, 0x40, 0x10, 0x52,  // ' '../
    0x3F, 0x06, 0x5B, 0x4F, 0x66, 0x6D, 0x7D, 0x07, 0x7F, 0x6F, 0x09, 0x0D, 0x61, 0x48, 0x43, 0x53,  // 0..?
    0x5F, 0x77, 0x7C, 0x39, 0x5E, 0x79, 0x71, 0x3D, 0x76, 0x30, 0x1E, 0x75, 0x38, 0x15, 0x37, 0x3F,  // @..O
    0x73, 0x6B, 0x33, 0x6D, 0x78, 0x3E, 0x3E, 0x2A, 0x76, 0x6E, 0x5B, 0x39, 0x64, 0x0F, 0x23, 0x08,  // P.._
    0x02, 0x5F, 0x7C, 0x58, 0x5E, 0x7B, 0x71, 0x6F, 0x74, 0x10, 0x0C, 0x75, 0x30, 0x14, 0x54, 0x5C,  // `..o
    0x73, 0x67, 0x50, 0x6D, 0x78, 0x1C, 0x1C, 0x14, 0x76, 0x6E, 0x5B, 0x46, 0x30, 0x70, 0x01,        // p..~
};

// Cyrillic А..Я approximations; lower case uses the same glyphs.
constexpr uint8_t kCyrillic[32] = {
    0x77, 0x7D, 0x7F, 0x31, 0x5E, 0x79, 0x76, 0x4F,  // А Б В Г Д Е Ж З
    0x3E, 0x3E, 0x75, 0x37, 0x15, 0x76, 0x3F, 0x37,  // И Й К Л М Н О П
    0x73, 0x39, 0x78, 0x6E, 0x71, 0x76, 0x3E, 0x66,  // Р С Т У Ф Х Ц Ч
    0x2A, 0x2A, 0x7C, 0x7C, 0x7C, 0x4F, 0x3F, 0x67,  // Ш Щ Ъ Ы Ь Э Ю Я
};

uint8_t glyph_for(uint32_t cp) {
  if (cp >= 0x20 && cp <= 0x7E) return kAscii[cp - 0x20] & 0x7F;
  if (cp >= 0x410 && cp <= 0x44F) return kCyrillic[(cp - 0x410) % 32];
  if (cp == 0x401 || cp == 0x451) return kCyrillic[5];  // Ё ё
  if (cp == 0xB0) return 0x63;                          // °
  return 0;
}

struct Cell {
  uint8_t glyph;
  bool colon_after;
};

// One cell per code point; ':' lights the colon after the previous cell. Malformed UTF-8 bytes
// become blank cells, one per byte.
std::vector<Cell> layout_text(const std::string &utf8) {
  std::vector<Cell> cells;
  const size_t n = utf8.size();
  for (size_t i = 0; i < n;) {
    unsigned char b = utf8[i];
    uint32_t cp = 0xFFFD;
    size_t len = b < 0x80 ? 1 : (b >> 5) == 0x6 ? 2 : (b >> 4) == 0xE ? 3 : (b >> 3) == 0x1E ? 4 : 0;
    if (len == 1) {
      cp = b;
    } else if (len && i + len <= n) {
      uint32_t v = b & (0x7F >> len);
      size_t k = 1;
      for (; k < len && (utf8[i + k] & 0xC0) == 0x80; k++) v = v << 6 | (utf8[i + k] & 0x3F);
      if (k == len) cp = v;
      else len = 0;
    } else {
      len = 0;
    }
    i += len ? len : 1;
    if (cp == ':') {
      if (!cells.empty()) cells.back().colon_after = true;
      continue;
    }
    cells.push_back({glyph_for(cp), false});
  }
  return cells;
}

// Faces from the "LED Studio" page (Text.html), positions left to right.
struct Face {
  const char *name;
  uint8_t g[kDigits];
};
constexpr Face kFaces[] = {
    {"Smiley", {0x40, 0x18, 0x0C, 0x40}},    {"Sad", {0x40, 0x21, 0x03, 0x40}},
    {"Neutral", {0x40, 0x40, 0x40, 0x40}},   {"Surprised", {0x40, 0x09, 0x00, 0x40}},
    {"Angry", {0x41, 0x40, 0x40, 0x41}},     {"Wink", {0x40, 0x18, 0x0C, 0x00}},
    {"Big Smile", {0x40, 0x38, 0x1C, 0x40}}, {"Sleepy", {0x08, 0x40, 0x40, 0x08}},
    {"Dead", {0x74, 0x08, 0x08, 0x74}},      {"Cool", {0x22, 0x18, 0x0C, 0x22}},
    {"Love", {0x30, 0x18, 0x0C, 0x30}},      {"Robot", {0x3F, 0x09, 0x00, 0x3F}},
    {"UwU", {0x1C, 0x1C, 0x1C, 0x1C}},       {"UwU thin", {0x1C, 0x14, 0x14, 0x1C}},
    {"UwU happy", {0x23, 0x1C, 0x1C, 0x23}}, {"OwO", {0x23, 0x08, 0x08, 0x23}},
    {">w<", {0x46, 0x1C, 0x1C, 0x70}},
};
constexpr int kStaticFaces = int(sizeof kFaces / sizeof kFaces[0]);
constexpr int kFaceBlink = kStaticFaces, kFaceAll = kStaticFaces + 1, kFaceCount = kStaticFaces + 2;
constexpr uint8_t kBlinkOpen[kDigits] = {0x23, 0x08, 0x08, 0x23};
constexpr uint8_t kBlinkShut[kDigits] = {0x40, 0x08, 0x08, 0x40};
constexpr int64_t kBlinkPeriodMs = 3000, kBlinkShutMs = 200, kBlinkStepMs = 100;
constexpr int64_t kFaceCycleMs = 2000;
constexpr int64_t kTemporaryMs = 5000, kTemporaryMaxS = 3600;

}  // namespace

struct Clock::Impl {
  Impl(Loop &l, const Model &m) : loop(l), model(m) {}
  Loop &loop;
  const Model &model;

  int seg_fd[kDigits][kSegs];
  std::vector<int> colon_fd;
  std::vector<int> current_fds;
  // last written state (-1 = unknown)
  int seg_last[kDigits][kSegs];
  int colon_last = -1;
  int level_last = -1;
  int on_value = 255;  // value of a lit segment (dimmed level when there is no chip current)

  bool enabled = true;
  float user_bri = 1.f;
  float lux = -1.f;  // <0: unknown, no auto-dimming
  AmbientDimmer dimmer;
  int64_t timer_end = 0;     // monotonic ms of a running countdown; 0 = none
  int64_t timer_paused = 0;  // ms left of a paused countdown; 0 = none
  int64_t flash_until = 0;
  int flash_value = 0;
  std::string code;  // pairing code digits shown instead of anything else (empty = none)
  int64_t code_since = 0;
  uint64_t tick = 0;
  unsigned refreshes = 0;

  struct Content {
    enum Kind { NONE, TEXT, FACE } kind = NONE;
    std::vector<Cell> cells;  // TEXT
    int face = -1;            // FACE: index into face_names()
    int64_t since = 0;        // origin of scroll / animation phase
  };
  Content base, overlay;  // base: Text/Select entities; overlay: temporary layer (actions)
  int64_t overlay_until = 0;
  int scroll_ms = 400;
  uint64_t anim = 0;  // one-shot timer re-armed at each frame boundary of moving content

  ~Impl() {
    if (tick) loop.cancel_timer(tick);
    if (anim) loop.cancel_timer(anim);
    for (auto &d : seg_fd)
      for (int fd : d)
        if (fd >= 0) close(fd);
    for (int fd : colon_fd)
      if (fd >= 0) close(fd);
    for (int fd : current_fds) close(fd);
  }

  int open_led(const std::string &name) const {
    return ::open((model.clock_dir + name + "/brightness").c_str(), O_WRONLY | O_CLOEXEC);
  }

  std::string seg_name(int digit, int seg) const {
    return std::string(1, char(model.clock_seg_base + seg)) + std::to_string(model.clock_pos_base + digit);
  }

  void forget_written() {
    for (auto &d : seg_last)
      for (int &v : d) v = -1;
    colon_last = -1;
  }

  void set_seg(int d, int s, bool on) {
    int v = on ? on_value : 0;
    if (seg_last[d][s] == v || seg_fd[d][s] < 0) return;
    if (hwutil::write_int_fd(seg_fd[d][s], v)) seg_last[d][s] = v;
  }

  void set_colon(bool on) {
    int v = on ? on_value : 0;
    if (colon_last == v) return;
    for (int fd : colon_fd) hwutil::write_int_fd(fd, v);
    colon_last = v;
  }

  float ambient() const { return dimmer.factor(); }  // 0.02..1, 1 while lux is unknown

  void apply_level() {
    int lvl = int(std::lround(255.f * user_bri * ambient()));
    lvl = user_bri <= 0 ? 0 : std::max(kMinLevel, std::min(255, lvl));
    if (lvl == level_last) return;
    level_last = lvl;
    LOGD("clock: level=%d (user %.2f, lux %.1f)", lvl, user_bri, lux);
    if (model.clock_chip_current) {
      for (int fd : current_fds) hwutil::write_int_fd(fd, lvl);
    } else {
      on_value = lvl;  // lit segments are rewritten with the new value on the next refresh
      forget_written();
    }
  }

  void show(const uint8_t glyph[kDigits], bool colon, bool force = false) {
    const bool on = enabled || force;
    for (int d = 0; d < kDigits; d++)
      for (int s = 0; s < kSegs; s++) set_seg(d, s, on && (glyph[d] >> s & 1));
    set_colon(on && colon);
  }

  // Up to 4 digits shown as is; longer codes page through 3-digit groups ("123-", "-456"),
  // matching the 3-3 grouping the Sendspin spec asks for.
  void show_code(int64_t now) {
    uint8_t g[kDigits] = {0, 0, 0, 0};
    auto glyph = [](char c) -> uint8_t { return c >= '0' && c <= '9' ? kFont[c - '0'] : kMinus; };
    if (code.size() <= size_t(kDigits)) {
      for (size_t i = 0; i < code.size(); i++) g[kDigits - code.size() + i] = glyph(code[i]);
    } else {
      size_t groups = (code.size() + 2) / 3;
      size_t page = size_t((now - code_since) / kCodePageMs) % groups;
      std::string grp = code.substr(page * 3, 3);
      if (page == 0) {
        for (size_t i = 0; i < grp.size(); i++) g[i] = glyph(grp[i]);
        g[3] = kMinus;
      } else {
        g[0] = kMinus;
        for (size_t i = 0; i < grp.size(); i++) g[1 + i] = glyph(grp[i]);
      }
    }
    show(g, false, true);
  }

  static void two(uint8_t *g, int v, bool blank_lead) {
    g[0] = (blank_lead && v < 10) ? 0 : kFont[(v / 10) % 10];
    g[1] = kFont[v % 10];
  }

  const Content *shown_content() const {
    if (overlay.kind != Content::NONE) return &overlay;
    if (base.kind != Content::NONE) return &base;
    return nullptr;
  }

  int64_t anim_step(const Content &c) const {
    if (c.kind == Content::TEXT && c.cells.size() > size_t(kDigits)) return scroll_ms;
    if (c.kind == Content::FACE && c.face == kFaceBlink) return kBlinkStepMs;
    if (c.kind == Content::FACE && c.face == kFaceAll) return kFaceCycleMs;
    return 0;
  }

  void render(const Content &c, int64_t now, uint8_t g[kDigits], bool &colon) const {
    const int64_t t = now - c.since;
    if (c.kind == Content::TEXT) {
      const int n = int(c.cells.size());
      // Static text is centred; scrolling text enters from the right and ends on a blank frame.
      const int first = n <= kDigits ? -(kDigits - n) / 2 : int((t / scroll_ms) % (n + kDigits)) - (kDigits - 1);
      for (int p = 0; p < kDigits; p++) {
        int ci = first + p;
        if (ci < 0 || ci >= n) continue;
        g[p] = c.cells[ci].glyph;
        if (p == 1) colon = c.cells[ci].colon_after;
      }
    } else if (c.face == kFaceBlink) {
      memcpy(g, t % kBlinkPeriodMs < kBlinkPeriodMs - kBlinkShutMs ? kBlinkOpen : kBlinkShut, kDigits);
    } else if (c.face == kFaceAll) {
      memcpy(g, kFaces[(t / kFaceCycleMs) % kStaticFaces].g, kDigits);
    } else if (c.face >= 0 && c.face < kStaticFaces) {
      memcpy(g, kFaces[c.face].g, kDigits);
    }
  }

  // Frame timer for moving content; also fires when the temporary layer ends, so a single scroll
  // pass does not show the start of a second one until the next tick.
  void schedule_anim() {
    if (anim) {
      loop.cancel_timer(anim);
      anim = 0;
    }
    if (!tick) return;  // not opened
    const Content *c = shown_content();
    if (!c) return;
    const int64_t step = anim_step(*c);
    const int64_t now = Loop::now_ms();
    int64_t delay = step ? step - (now - c->since) % step : 0;
    if (c == &overlay) delay = std::max<int64_t>(1, step ? std::min(delay, overlay_until - now) : overlay_until - now);
    if (!delay) return;
    anim = loop.add_timer(delay, [this] {
      anim = 0;
      refresh();
      schedule_anim();
    });
  }

  void refresh() {
    int64_t now = Loop::now_ms();
    if (overlay.kind != Content::NONE && now >= overlay_until) {
      overlay = {};
      schedule_anim();
    }
    if (!code.empty()) return show_code(now);
    uint8_t g[kDigits] = {0, 0, 0, 0};
    bool colon = false;
    if (now < flash_until) {
      // Right-aligned number, '-' for negatives.
      int v = flash_value, n = std::abs(v);
      int pos = kDigits - 1;
      do {
        g[pos--] = kFont[n % 10];
        n /= 10;
      } while (n && pos >= 0);
      if (v < 0 && pos >= 0) g[pos] = kMinus;
    } else if (timer_end || timer_paused) {
      int64_t left = std::max<int64_t>(0, ((timer_end ? timer_end - now : timer_paused) + 999) / 1000);
      if (left >= 3600) {  // h:mm (hours >= 10 show as hh:mm)
        two(g, int(std::min<int64_t>(left / 3600, 99)), true);
        two(g + 2, int(left / 60 % 60), false);
      } else {
        two(g, int(left / 60), true);
        two(g + 2, int(left % 60), false);
      }
      colon = left > 0 || (now / 500) % 2 == 0;  // blink at 00:00
      if (left == 0 && (now / 500) % 2) memset(g, 0, sizeof g);
    } else if (const Content *c = shown_content()) {
      render(*c, now, g, colon);
    } else {
      time_t t = time(nullptr);
      struct tm tm;
      localtime_r(&t, &tm);
      two(g, tm.tm_hour, false);
      two(g + 2, tm.tm_min, false);
      timespec ts;
      clock_gettime(CLOCK_REALTIME, &ts);
      colon = ts.tv_nsec < 500000000;  // on for the first half of every second
    }
    show(g, colon);
  }
};

Clock::Clock(Loop &loop, const Model &model) : impl_(new Impl(loop, model)) {
  for (auto &d : impl_->seg_fd)
    for (int &fd : d) fd = -1;
  for (auto &d : impl_->seg_last)
    for (int &v : d) v = -1;
}

Clock::~Clock() = default;

bool Clock::open() {
  auto &d = *impl_;
  int found = 0;
  for (int dig = 0; dig < kDigits; dig++)
    for (int s = 0; s < kSegs; s++) {
      std::string name = d.seg_name(dig, s);
      d.seg_fd[dig][s] = d.open_led(name);
      if (d.seg_fd[dig][s] >= 0) found++;
      else LOGW("clock: missing segment %s", name.c_str());
    }
  if (!found) {
    LOGE("clock: no clock segments found");
    return false;
  }
  for (const auto &name : d.model.clock_colon) {
    int fd = d.open_led(name);
    if (fd >= 0) d.colon_fd.push_back(fd);
  }
  for (const auto &aux : d.model.clock_aux) {
    int fd = d.open_led(aux);
    if (fd >= 0) {
      hwutil::write_int_fd(fd, 0);
      close(fd);
    }
  }
  if (d.model.clock_chip_current) {
    // Global current of the clock driver chips, found through the LED device links of the
    // leftmost and rightmost digit (one chip each on the Midi).
    for (const std::string &seg : {d.seg_name(0, 0), d.seg_name(kDigits - 1, 0)}) {
      std::string dev = d.model.clock_dir + seg + "/device/current";
      char real[256];
      if (!realpath(dev.c_str(), real)) continue;
      bool dup = false;
      for (int fd : d.current_fds) {
        char p[64], r2[256];
        snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
        ssize_t n = readlink(p, r2, sizeof r2 - 1);
        if (n > 0 && std::string(r2, size_t(n)) == real) dup = true;
      }
      if (dup) continue;
      int fd = ::open(real, O_WRONLY | O_CLOEXEC);
      if (fd >= 0) d.current_fds.push_back(fd);
    }
  }
  LOGI("clock: %d segments, %zu colon LEDs, %s dimming", found, d.colon_fd.size(),
       d.model.clock_chip_current ? "chip current" : "segment brightness");
  d.apply_level();
  d.refresh();
  // Every ~5 s rewrite all segments even if unchanged (sysfs state may be changed by others);
  // counted on the tick only, animation frames refresh in between.
  d.tick = d.loop.add_timer(kTickMs, [&d] {
    if (++d.refreshes % 20 == 0) d.forget_written();
    d.refresh();
  }, kTickMs);
  d.schedule_anim();
  return true;
}

void Clock::set_enabled(bool on) {
  impl_->enabled = on;
  impl_->refresh();
}

void Clock::set_brightness(float b) {
  impl_->user_bri = std::max(0.f, std::min(1.f, b));
  impl_->apply_level();
  impl_->refresh();
}

void Clock::set_ambient_lux(float lux) {
  impl_->lux = std::max(0.f, lux);
  impl_->dimmer.update(impl_->lux);
  impl_->apply_level();
  impl_->refresh();
}

void Clock::set_timer(int64_t ms_left, bool running) {
  impl_->timer_end = ms_left > 0 && running ? Loop::now_ms() + ms_left : 0;
  impl_->timer_paused = ms_left > 0 && !running ? ms_left : 0;
  impl_->refresh();
}

void Clock::flash_number(int value) {
  impl_->flash_value = std::max(-999, std::min(9999, value));
  impl_->flash_until = Loop::now_ms() + kFlashMs;
  impl_->refresh();
}

void Clock::show_code(const std::string &digits) {
  impl_->code = digits;
  impl_->code_since = Loop::now_ms();
  impl_->refresh();
}

void Clock::clear_code() {
  impl_->code.clear();
  impl_->refresh();
}

void Clock::set_text(const std::string &utf8) {
  auto &d = *impl_;
  std::vector<Cell> cells = layout_text(utf8);
  if (cells.empty()) {
    // Non-empty text without cells (only ':') still replaces the face, as the controller does.
    if (d.base.kind == Impl::Content::TEXT || !utf8.empty()) d.base = {};
  } else {
    d.base = {Impl::Content::TEXT, std::move(cells), -1, Loop::now_ms()};
  }
  d.refresh();
  d.schedule_anim();
}

void Clock::set_face(int index) {
  auto &d = *impl_;
  if (index < 0 || index >= kFaceCount) {
    if (d.base.kind == Impl::Content::FACE) d.base = {};
  } else {
    d.base = {Impl::Content::FACE, {}, index, Loop::now_ms()};
  }
  d.refresh();
  d.schedule_anim();
}

void Clock::set_scroll_ms(int ms) {
  impl_->scroll_ms = std::max(100, std::min(2000, ms));
  impl_->refresh();
  impl_->schedule_anim();
}

void Clock::show_temporary_text(const std::string &utf8, int seconds) {
  auto &d = *impl_;
  std::vector<Cell> cells = layout_text(utf8);
  const int64_t now = Loop::now_ms();
  if (cells.empty()) {
    d.overlay = {};
  } else {
    const int64_t n = int64_t(cells.size());
    d.overlay_until = now + (seconds > 0   ? std::min<int64_t>(seconds, kTemporaryMaxS) * 1000
                             : n > kDigits ? (n + kDigits) * d.scroll_ms
                                           : kTemporaryMs);
    d.overlay = {Impl::Content::TEXT, std::move(cells), -1, now};
  }
  d.refresh();
  d.schedule_anim();
}

void Clock::show_temporary_face(int index, int seconds) {
  auto &d = *impl_;
  const int64_t now = Loop::now_ms();
  if (index < 0 || index >= kFaceCount) {
    d.overlay = {};
  } else {
    d.overlay_until = now + (seconds > 0 ? std::min<int64_t>(seconds, kTemporaryMaxS) * 1000 : kTemporaryMs);
    d.overlay = {Impl::Content::FACE, {}, index, now};
  }
  d.refresh();
  d.schedule_anim();
}

const std::vector<std::string> &Clock::face_names() {
  static const std::vector<std::string> names = [] {
    std::vector<std::string> v;
    for (const Face &f : kFaces) v.emplace_back(f.name);
    v.emplace_back("Blink");
    v.emplace_back("All faces");
    return v;
  }();
  return names;
}

}  // namespace cs
