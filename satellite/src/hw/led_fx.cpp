#include "hw/led_fx.h"

#include <algorithm>
#include <cmath>
#include <iterator>

namespace cs::fx {

namespace {

constexpr float kTau = 6.2831853f;

// Circular distance between positions a and b on a ring of n LEDs.
float cdist(float a, float b, float n) {
  float d = std::fmod(std::fabs(a - b), n);
  return std::min(d, n - d);
}

// Distance (in Midi LEDs) of LED i to the nearest of the four symmetry points 0, n/4, n/2, 3n/4.
float quarter_dist(int i, int n) {
  const float q = n / 4.f, r = std::fmod(float(i), q);
  return std::min(r, q - r) * 24.f / n;
}

uint32_t xorshift(uint32_t &s) {
  s ^= s << 13;
  s ^= s >> 17;
  s ^= s << 5;
  return s;
}

float hash(int32_t x, uint32_t y) {
  uint32_t h = uint32_t(x) * 374761393u + y * 668265263u;
  h = (h ^ (h >> 13)) * 1274126177u;
  return float((h ^ (h >> 16)) & 0xFFFFFF) / float(0xFFFFFF);
}

// y is the time axis: kept in double so that its fraction stays exact however long the phase runs.
float noise1(float x, double y) {
  const float fx = std::floor(x);
  const double fy = std::floor(y);
  const int32_t ix = int32_t(fx);
  const uint32_t iy = uint32_t(int64_t(fy));
  float u = x - fx, v = float(y - fy);
  u = u * u * (3 - 2 * u);
  v = v * v * (3 - 2 * v);
  float a = hash(ix, iy), b = hash(ix + 1, iy), c = hash(ix, iy + 1), d = hash(ix + 1, iy + 1);
  return (a + (b - a) * u) + ((c + (d - c) * u) - (a + (b - a) * u)) * v;
}

// k = 0.55 + 0.45·cos(2πt/period): the stock "breathing" between ~10 % and full.
float breathe(double t, double period) {
  return 0.55f + 0.45f * std::cos(kTau * float(std::fmod(t, period) / period));
}

// Listening after its 420 ms opening: 100 % <-> 40 % with a 1.54 s period.
float listen_k(float t) { return 0.70f + 0.30f * std::cos(kTau * float(std::fmod(t - 420, 1540.f) / 1540)); }

// Stock alice_* violets: Midi strip; Midi logo and Lite 2 ring. Timer green of ring / start.
constexpr uint32_t kViolet = 0x370fff, kVioletSmall = 0x6b0fff;
constexpr uint32_t kTimerGreen = 0x00dc00, kTimerStartGreen = 0x00e600, kMuteRed = 0xd20000;

void fill(Col *out, int n, Col c) { std::fill(out, out + n, c); }

// Palette hue in degrees (stock "blink" preset sequence), 15 s per entry, 3 s hue crossfade.
float palette(double t) {
  static const float kHues[] = {290, 340, 20, 355, 290, 250, 180, 250, 290, 340, 20, 48, 115, 180, 250};
  constexpr int kCount = sizeof(kHues) / sizeof(kHues[0]);
  constexpr double kEntry = 15, kFade = 3;
  double cycle = std::fmod(std::max(0.0, t), kEntry * kCount);
  int i = int(cycle / kEntry);
  double u = cycle - i * kEntry;
  float h0 = kHues[i], h1 = kHues[(i + 1) % kCount];
  if (u < kEntry - kFade) return h0;
  float dh = std::fmod(h1 - h0 + 540.f, 360.f) - 180.f;
  return h0 + dh * float((u - (kEntry - kFade)) / kFade);
}

Col hsvd(float deg, float s, float v) { return hsv(deg / 360.f, s, v); }

}  // namespace

Col rgb(int r, int g, int b) { return {r / 255.f, g / 255.f, b / 255.f}; }

Col lerp(Col a, Col b, float t) { return {a.r + (b.r - a.r) * t, a.g + (b.g - a.g) * t, a.b + (b.b - a.b) * t}; }

Col cmax(Col a, Col b) { return {std::max(a.r, b.r), std::max(a.g, b.g), std::max(a.b, b.b)}; }

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

Col stock(uint32_t rgb, float k) {
  k = std::max(0.f, k);
  return {std::sqrt(float((rgb >> 16) & 0xFF) / 255.f * k), std::sqrt(float((rgb >> 8) & 0xFF) / 255.f * k),
          std::sqrt(float(rgb & 0xFF) / 255.f * k)};
}

float clamp01(float v) { return std::max(0.f, std::min(1.f, v)); }

float smoothstep(float e0, float e1, float x) {
  float t = clamp01((x - e0) / (e1 - e0));
  return t * t * (3 - 2 * t);
}

float wave(double t, double period) { return 0.5f - 0.5f * std::cos(float(std::fmod(t, period) / period) * kTau); }

float noise(float x, double y) { return 0.67f * noise1(x, y) + 0.33f * noise1(2 * x, 2 * y); }

// ---------------------------------------------------------------------------------------------
// Voice states

void voice(Voice v, int64_t t, Col *out, int n) {
  const float ft = float(t);
  const float half = n / 2.f;
  switch (v) {
    case Voice::LISTENING:
      if (!wide(n)) {
        fill(out, n, stock(kVioletSmall, breathe(ft, 1600)));
        return;
      }
      // Sweep from LED 0 both ways, meeting opposite in 420 ms; then breathe 100 % <-> 40 %.
      for (int i = 0; i < n; i++) {
        float k = ft < 420 ? clamp01(ft / 420 * half - cdist(float(i), 0, float(n)) + 1) : listen_k(ft);
        out[i] = stock(kViolet, k);
      }
      return;
    case Voice::THINKING:
      if (!wide(n)) {
        // One point circling over a dim violet background.
        float c = float(t % 1620) / 1620 * n;
        for (int i = 0; i < n; i++)
          out[i] = cmax(stock(0x0a0219), stock(0x640eee, std::max(0.f, 1 - cdist(float(i), c, float(n)))));
        return;
      }
      {
        // One band (9 full LEDs, 3-LED tails) circling once per 1.5 s.
        float c = float(t % 1500) / 1500 * n;
        for (int i = 0; i < n; i++) {
          float d = cdist(float(i), c, float(n));
          float k = d <= 4.5f ? 1 : d < 7.5f ? ((7.5f - d) / 3) * ((7.5f - d) / 3) : 0;
          out[i] = stock(kViolet, k);
        }
      }
      return;
    case Voice::SPEAKING:
      if (!wide(n)) {
        fill(out, n, stock(0x6c0fff, breathe(ft, 1000)));
        return;
      }
      {
        // Blob opposite LED 0 whose width flickers irregularly (two incommensurate periods).
        float w = 1.5f + 3 * (0.5f + 0.5f * std::sin(kTau * ft / 530) * (0.6f + 0.4f * std::sin(kTau * ft / 1600)));
        for (int i = 0; i < n; i++)
          out[i] = stock(0x7b0ee8, smoothstep(w + 1, w - 1, cdist(float(i), half, float(n))));
      }
      return;
    case Voice::ERROR: {
      // Three pulses: 80 ms rise, exponential fade.
      float tau = float(t % 1180);
      float k = t >= kErrorMs ? 0 : tau < 80 ? tau / 80 : std::exp(-(tau - 80) / 300);
      fill(out, n, stock(0xef0000, k));
      return;
    }
    case Voice::TIMER_RING: {
      float k = breathe(ft, 1300);
      if (!wide(n)) {
        for (int i = 0; i < n; i++)
          out[i] = stock(kTimerGreen,
                         0.55f + 0.45f * std::cos(kTau * (float(std::fmod(ft, 1300.f) / 1300) - float(i) / n)));
        return;
      }
      // Two opposite blobs swaying around LEDs 2.5 and 14.5.
      float c1 = 2.5f + std::sin(kTau * ft / 1300) * n / 24.f;
      for (int i = 0; i < n; i++) {
        float d = std::min(cdist(float(i), c1, float(n)), cdist(float(i), c1 + half, float(n))) * 24.f / n;
        out[i] = stock(kTimerGreen, k * std::max(0.f, 1 - d / 2.5f));
      }
      return;
    }
  }
}

void voice_logo(Voice v, int64_t t, Col *out, int n) {
  const float ft = float(t);
  switch (v) {
    case Voice::LISTENING: {
      fill(out, n, stock(kVioletSmall, ft < 420 ? ft / 420 : listen_k(ft)));
      return;
    }
    case Voice::THINKING: {
      float c = float(t % 1500) / 1500 * n;
      for (int i = 0; i < n; i++) {
        float d = cdist(float(i), c, float(n));
        out[i] = stock(kVioletSmall, d <= 1 ? 1 : d < 2 ? (2 - d) * (2 - d) : 0);
      }
      return;
    }
    case Voice::SPEAKING: fill(out, n, stock(kVioletSmall, breathe(ft, 1240))); return;
    case Voice::ERROR: voice(v, t, out, n); return;
    case Voice::TIMER_RING: fill(out, n, stock(kTimerGreen, breathe(ft, 1300))); return;
  }
}

// ---------------------------------------------------------------------------------------------
// One-shots

void timer_started(int64_t t, Col *out, int n) {
  const float ft = float(std::min(t, kTimerStartedMs));
  if (!wide(n)) {
    fill(out, n, stock(kTimerStartGreen, 1 - ft / kTimerStartedMs));
    return;
  }
  // Green at the four symmetry points, shrinking to nothing over 15 s.
  const float hw = 2.5f * (1 - ft / kTimerStartedMs);
  const float point = ft < 14400 ? 1 : (kTimerStartedMs - ft) / 600;
  for (int i = 0; i < n; i++) {
    const float d = quarter_dist(i, n);
    out[i] = stock(kTimerStartGreen, d < 0.01f ? point : clamp01(hw - d + 0.5f));
  }
}

void mute_sweep(bool on, int64_t t, Col *out, int n) {
  const float ft = float(std::min(t, kMuteSweepMs));
  if (!wide(n)) {
    fill(out, n, on ? stock(kMuteRed, 1 - ft / kMuteSweepMs) : Col{});
    return;
  }
  // Two red heads from LED 0 to the opposite LED (unmute: the other way), with 4-LED tails.
  const float half = n / 2.f, h = std::min(1.f, ft / 500) * half;
  const float tail = ft > 500 ? (kMuteSweepMs - ft) / 100 : 1;
  for (int i = 0; i < n; i++) {
    float d0 = cdist(float(i), 0, float(n));
    if (!on) d0 = half - d0;
    out[i] = stock(kMuteRed, d0 <= h ? clamp01(1 - (h - d0) / 4 * 24.f / n) * tail : 0);
  }
}

Col mute_steady() { return stock(0x200000); }

Col mute_logo() { return stock(0x720000); }

void volume(float v, Col *out, int n) {
  struct Point {
    float v;
    uint32_t c;
  };
  static const Point kWide[] = {{0, 0x000d00},     {0.067f, 0x001d01}, {0.2f, 0x003d02},
                                {0.33f, 0x006103}, {0.47f, 0x00a002},  {0.6f, 0x00df01},
                                {0.73f, 0x6dda00}, {0.87f, 0xf44100},  {1, 0xf80100}};
  static const Point kShort[] = {{0, 0x116111},        {1 / 6.f, 0x3f9f25}, {1 / 3.f, 0x71d50e}, {0.5f, 0xc9fa00},
                                 {2 / 3.f, 0xecbf00}, {5 / 6.f, 0xec6a00}, {1, 0xf80000}};
  v = clamp01(v);
  const Point *pts = wide(n) ? kWide : kShort;
  const size_t np = wide(n) ? std::size(kWide) : std::size(kShort);
  // Per-channel linear interpolation of the raw values.
  size_t j = 1;
  while (j < np - 1 && v > pts[j].v) j++;
  float f = clamp01((v - pts[j - 1].v) / (pts[j].v - pts[j - 1].v));
  uint32_t raw = 0;
  for (int sh = 16; sh >= 0; sh -= 8) {
    float a = float((pts[j - 1].c >> sh) & 0xFF), b = float((pts[j].c >> sh) & 0xFF);
    raw |= uint32_t(std::lround(a + (b - a) * f)) << sh;
  }
  if (!wide(n)) {
    fill(out, n, stock(raw));
    return;
  }
  // Grows from the four symmetry points.
  const float h = 3.5f * v;
  for (int i = 0; i < n; i++) {
    const float d = quarter_dist(i, n);
    out[i] = stock(raw, d < 0.01f ? 1 : clamp01(h - d + 0.5f));
  }
}

// ---------------------------------------------------------------------------------------------
// Effects

bool effect(const std::string &name, Col c, float bri, double t, double dt, EffectState &st, Col *out, int n) {
  if (name == "Rainbow") {
    for (int i = 0; i < n; i++) out[i] = hsv(float(std::fmod(t / 6, 1.0)) + float(i) / n, 1, 1) * bri;
    return true;
  }
  if (name == "Breathe") {
    fill(out, n, c * (bri * (0.1f + 0.9f * wave(t, 4))));
    return true;
  }
  if (name == "Color loop") {
    fill(out, n, hsv(float(std::fmod(t / 20, 1.0)), 1, 1) * bri);
    return true;
  }
  if (name == "Comet") {
    const float head = float(std::fmod(t / 2, 1.0)) * n, len = std::max(1.5f, 0.4f * n);
    for (int i = 0; i < n; i++) {
      float d = std::fmod(head - i + 2.f * n, float(n));  // distance behind the head
      float k = d < len ? (1 - d / len) * (1 - d / len) : 0;
      out[i] = c * (bri * k);
    }
    return true;
  }
  if (name == "Twinkle") {
    if (st.age.size() != size_t(n)) st.age.assign(size_t(n), -1);
    const double rate = n <= 3 ? 0.8 : 0.6;  // sparkles per LED per second
    for (int i = 0; i < n; i++) {
      float &a = st.age[size_t(i)];
      if (a >= 0) {
        a += float(dt);
        if (a >= 0.95f) a = -1;
      } else if (double(xorshift(st.rng)) / 4294967296.0 < rate * dt) {
        a = 0;
      }
      float env = a < 0 ? 0 : a < 0.15f ? a / 0.15f : std::exp(-3 * (a - 0.15f) / 0.8f);
      out[i] = c * (bri * std::max(0.08f, env));
    }
    return true;
  }
  if (name == "Fire") {
    for (int i = 0; i < n; i++) {
      float heat = 0.35f + 0.65f * noise(0.35f * i, 2.5 * t);
      Col col = heat < 0.33f   ? stock(0xff0000, heat / 0.33f)
                : heat < 0.66f ? lerp(stock(0xff0000), stock(0xff6000), (heat - 0.33f) / 0.33f)
                               : lerp(stock(0xff6000), stock(0xffb000), (heat - 0.66f) / 0.34f);
      out[i] = col * bri;
    }
    return true;
  }
  if (name == "Aurora") {
    for (int i = 0; i < n; i++) {
      float x = float(i) / n;
      float hue = 165 + 55 * std::sin(kTau * (x + float(std::fmod(t / 17, 1.0)))) +
                  40 * std::sin(kTau * (2 * x - float(std::fmod(t / 11, 1.0))));
      out[i] = hsvd(hue, 0.9f, 0.35f + 0.65f * noise(0.2f * i, 0.3 * t)) * bri;
    }
    return true;
  }
  if (name == "Lava lamp") {
    static const uint32_t kCol[4] = {0xfe3200, 0xfe0065, 0x5800fe, 0x00fe1e};
    static const double kPhase[4] = {0, 0.25, 0.5, 0.75}, kVel[4] = {0.013, -0.021, 0.017, -0.029};
    static const float kSigma[4] = {0.12f, 0.10f, 0.14f, 0.09f};
    for (int i = 0; i < n; i++) {
      Col sum, nearest;
      float wsum = 0, dmin = 1e9f;
      for (int j = 0; j < 4; j++) {
        float centre = float(std::fmod(kPhase[j] + kVel[j] * t, 1.0)) * n;
        float d = cdist(float(i), centre + float(n), float(n));
        float s = std::max(0.6f, n * kSigma[j]);
        float w = std::exp(-d * d / (2 * s * s));
        sum = sum + stock(kCol[j]) * w;
        wsum += w;
        if (d < dmin) dmin = d, nearest = stock(kCol[j]);
      }
      out[i] = cmax(sum * (1 / std::max(1.f, wsum)), nearest * 0.05f) * bri;
    }
    return true;
  }
  if (name == "Candle") {
    float g = 0.88f + 0.08f * noise(0, 0.6 * t) + 0.04f * noise(7, 8 * t);
    for (int i = 0; i < n; i++) {
      float v = 1 - 0.08f * (0.5f + 0.5f * std::cos(kTau * i / 6 + float(std::fmod(0.9 * t, double(kTau)))));
      out[i] = stock(0xffb200, g * v) * bri;
    }
    return true;
  }
  if (name == "Night light") {
    fill(out, n, stock(0xd4ffbf) * bri);
    return false;
  }
  fill(out, n, c * bri);
  return false;
}

// ---------------------------------------------------------------------------------------------
// Music

void music_step(MusicState &st, const MusicInput &in, bool beat, double t, double dt) {
  if (beat) st.impulse += 1;
  st.impulse *= std::exp(-dt / 0.4);
  st.theta = std::fmod(st.theta + (0.15 + 0.6 * in.loudness + st.impulse) * dt, 1.0);
  if (t >= st.next_pick) {
    st.target = double(xorshift(st.rng)) / 4294967296.0;
    st.next_pick = t + 5;
  }
  double diff = std::fmod(st.target - st.p + 1.5, 1.0) - 0.5;  // shortest way, turns
  double step = 0.1 * dt;
  st.p = std::fmod(st.p + (std::fabs(diff) <= step ? diff : std::copysign(step, diff)) + 1.0, 1.0);
}

void music(Style s, const MusicState &st, const MusicInput &in, double t, Col *out, int n) {
  const float H = palette(t), L = clamp01(in.loudness);
  const auto &b = in.bands;
  switch (s) {
    case Style::SPECTRUM:
      if (!wide(n)) {
        const float v[3] = {(b[0] + b[1] + b[2]) / 3, (b[3] + b[4] + b[5]) / 3, (b[6] + b[7]) / 2};
        for (int i = 0; i < n; i++) out[i] = hsvd(H + 60.f * (i % 3), 1, v[i % 3]);
        return;
      }
      // Bass at LED 0, highs opposite, mirrored on both sides.
      for (int i = 0; i < n; i++) {
        int d0 = std::min(i, n - i);
        int band = std::min(7, d0 * 8 / (n / 2 + 1));
        out[i] = hsvd(H + 25.f * band, 1, b[size_t(band)]);
      }
      return;
    case Style::PULSE: {
      const float v = std::min(1.f, 0.15f + 0.85f * L + 0.3f * in.flash);
      if (!wide(n)) {
        fill(out, n, hsvd(H, 0.9f, v));
        return;
      }
      const float w = 1.5f + L * 10.5f * n / 24.f;
      for (int i = 0; i < n; i++)
        out[i] = hsvd(H, 0.9f, v * clamp01(w - cdist(float(i), n / 2.f, float(n)) + 0.5f));
      return;
    }
    case Style::DISCO: {
      const float v = 0.25f + 0.75f * L, reach = wide(n) ? 2.f : 1.f;
      for (int i = 0; i < n; i++) {
        Col c;
        for (int j = 0; j < 3; j++) {
          float pos = float(std::fmod(st.theta + j / 3.0, 1.0)) * n;
          c = cmax(c, hsvd(H + 120.f * j, 1, v * clamp01(reach - cdist(float(i), pos, float(n)))));
        }
        out[i] = c;
      }
      return;
    }
    case Style::POLAR: {
      const Col bg = hsvd(H + 180, 0.8f, 0.12f);
      const float half = n / 2.f, p = float(st.p) * n;
      for (int i = 0; i < n; i++) {
        float d = cdist(float(i), p, float(n));
        int band = std::min(7, int(d * 8 / (half + 1)));
        out[i] = cmax(bg, hsvd(H, 1, b[size_t(band)] * (1 - 0.5f * d / half)));
      }
      return;
    }
  }
}

}  // namespace cs::fx
