#include "hw/clock_fx.h"

#include <algorithm>
#include <cmath>
#include <cstddef>

namespace cs::clockfx {

namespace {

constexpr int A = 0, B = 1, C = 2, D = 3, E = 4, F = 5, G = 6;

struct Key {
  int16_t t;
  uint8_t v;
};

// Piecewise linear; two keys with the same t make a step.
uint8_t at(const Key *k, size_t n, int64_t t) {
  if (t < k[0].t) return k[0].v;
  size_t i = 0;
  while (i + 1 < n && k[i + 1].t <= t) i++;
  if (i + 1 >= n) return k[i].v;
  const double x = double(t - k[i].t) / double(k[i + 1].t - k[i].t);
  return uint8_t(std::lround(k[i].v + (k[i + 1].v - k[i].v) * x));
}

template <size_t N>
uint8_t at(const Key (&k)[N], int64_t t) {
  return at(k, N, t);
}

// Envelopes measured on the stock patterns (≤ 10/255 off). Outer = digits 0 and 3, inner = 1 and 2.
constexpr Key kListenOuter[] = {{0, 0},     {20, 36},   {40, 198}, {60, 223}, {180, 215},
                                {200, 192}, {260, 54},  {280, 26}, {320, 4},  {1000, 0}};
constexpr Key kListenInner[] = {{0, 0},     {60, 6},    {80, 81},   {100, 182}, {120, 218}, {140, 223},
                                {340, 222}, {400, 208}, {460, 174}, {580, 70},  {700, 14},  {1000, 0}};
constexpr Key kListenCenter[] = {{0, 0}, {80, 0}, {180, 74}, {300, 102}, {760, 81}, {920, 50}, {980, 23}, {1000, 0}};

constexpr Key kThinkG0[] = {{0, 25},     {100, 47},   {200, 159},  {220, 150},  {280, 11},   {300, 0},
                            {1180, 1},   {1200, 17},  {1260, 145}, {1300, 165}, {1540, 48},  {1800, 25}};
constexpr Key kThinkG1[] = {{0, 3},      {140, 14},   {180, 59},   {220, 157},  {240, 169},  {260, 149},
                            {300, 49},   {340, 5},    {1120, 3},   {1160, 23},  {1200, 129}, {1220, 166},
                            {1240, 174}, {1280, 140}, {1340, 50},  {1380, 25},  {1500, 6},   {1800, 3}};
constexpr Key kThinkCenter[] = {{0, 3},      {180, 7},    {200, 22},   {260, 171}, {280, 173}, {300, 151},
                                {360, 29},   {400, 5},    {1080, 4},   {1120, 26}, {1140, 61}, {1180, 162},
                                {1200, 171}, {1220, 155}, {1280, 33},  {1320, 8},  {1800, 3}};
constexpr Key kThinkG2[] = {{0, 3},     {220, 6},    {240, 23},   {280, 136},  {300, 168},  {320, 174},
                            {360, 139}, {420, 47},   {500, 10},   {1040, 8},   {1080, 34},  {1160, 159},
                            {1180, 149}, {1220, 54}, {1240, 19},  {1260, 5},   {1800, 3}};
constexpr Key kThinkG3[] = {{0, 0},     {260, 2},    {280, 21},   {340, 148},  {380, 167},  {580, 47},
                            {680, 25},  {960, 19},   {1020, 45},  {1140, 144}, {1160, 121}, {1200, 15},
                            {1220, 1},  {1800, 0}};

constexpr Key kSpeakOuter[] = {{0, 0},      {800, 5},   {860, 38}, {960, 168}, {1000, 194},
                               {1040, 163}, {1100, 46}, {1140, 4}, {1340, 0}};
constexpr Key kSpeakInner[] = {{0, 0},     {140, 4},   {220, 62},   {300, 84},   {360, 78},  {500, 18},
                               {660, 7},   {840, 127}, {1000, 204}, {1080, 163}, {1280, 9},  {1340, 0}};
constexpr Key kSpeakCenter[] = {{0, 9},     {100, 40},  {140, 74},   {180, 129},  {280, 180},  {360, 168},
                                {520, 53},  {640, 19},  {1000, 204}, {1040, 189}, {1240, 51},  {1340, 9}};

// Eyes on digits 0 and 3: g is the eye, a/b/f the lids; the right eye winks late.
constexpr Key kBootEye[] = {{0, 64}, {60, 255}, {4880, 255}};
constexpr Key kBootLidLeft[] = {{0, 0},      {60, 0},      {140, 255},  {1040, 255}, {1040, 0},   {1220, 0},
                                {1220, 255}, {1400, 255},  {1400, 0},   {1560, 0},   {1560, 255}, {4880, 255}};
constexpr Key kBootLidRight[] = {{0, 0},      {60, 0},     {140, 255},  {1040, 255}, {1040, 0},
                                 {1220, 0},   {1220, 255}, {1400, 255}, {1400, 0},   {1560, 0},
                                 {1560, 255}, {3800, 255}, {3800, 0},   {4220, 0},   {4220, 255},
                                 {4880, 255}};

constexpr Key kPause[] = {{0, 0}, {280, 255}, {1960, 255}, {2200, 0}};

// Vertical half-digit bars: (digit, right half b+c / left half e+f).
struct Bar {
  int digit;
  bool right;
};

void bar(Frame &f, Bar b, uint8_t v) {
  if (b.right) f.seg[b.digit][B] = f.seg[b.digit][C] = v;
  else f.seg[b.digit][E] = f.seg[b.digit][F] = v;
}

float ease(double x) { return float((1 - std::cos(M_PI * x)) / 2); }

// Ringing: the bars light pairwise from the centre outwards, each ramping up, holding and down.
uint8_t ring_bar(int64_t u) {
  if (u < 0 || u >= 800) return 0;
  double v;
  if (u < 160) v = 128 * ease(u / 160.0);
  else if (u < 320) v = 128 + 127 * ease((u - 160) / 160.0);
  else if (u < 480) v = 255;
  else if (u < 640) v = 255 - 127 * ease((u - 480) / 160.0);
  else v = 128 - 128 * ease((u - 640) / 160.0);
  return uint8_t(std::lround(v));
}

// Next / previous: a column sweep, each column flashing on and fading out.
uint8_t sweep_column(int c, int64_t t) {
  const int64_t on = 20 + 160 * c;
  if (t < on) return 0;
  if (t < on + 160) return 255;
  if (t < on + 480) return uint8_t(std::lround(128 - 128.0 * double(t - on - 160) / 320));
  return 0;
}

}  // namespace

bool loops(Anim a) { return a <= Anim::RINGING; }

int64_t duration_ms(Anim a) {
  switch (a) {
    case Anim::LISTENING: return 1000;
    case Anim::THINKING: return 1800;
    case Anim::SPEAKING: return 1340;
    case Anim::RINGING: return 1780;
    case Anim::BOOT: return 4880;
    case Anim::PAUSE: return 2200;
    case Anim::NEXT:
    case Anim::PREVIOUS: return 1500;
  }
  return 0;
}

void render(Anim a, int64_t t, Frame &f) {
  f = {};
  const int64_t dur = duration_ms(a);
  if (t < 0) t = 0;
  if (loops(a)) t %= dur;
  else if (t >= dur) return;
  switch (a) {
    case Anim::LISTENING:
      f.seg[0][G] = f.seg[3][G] = at(kListenOuter, t);
      f.seg[1][G] = f.seg[2][G] = at(kListenInner, t);
      f.center = at(kListenCenter, t);
      break;
    case Anim::THINKING:
      f.seg[0][G] = at(kThinkG0, t);
      f.seg[1][G] = at(kThinkG1, t);
      f.center = at(kThinkCenter, t);
      f.seg[2][G] = at(kThinkG2, t);
      f.seg[3][G] = at(kThinkG3, t);
      break;
    case Anim::SPEAKING:
      f.seg[0][G] = f.seg[3][G] = at(kSpeakOuter, t);
      f.seg[1][G] = f.seg[2][G] = at(kSpeakInner, t);
      f.center = at(kSpeakCenter, t);
      break;
    case Anim::RINGING: {
      static constexpr Bar kBars[4][2] = {
          {{1, true}, {2, false}}, {{1, false}, {2, true}}, {{0, true}, {3, false}}, {{0, false}, {3, true}}};
      for (int k = 0; k < 4; k++) {
        const uint8_t v = ring_bar(t - (480 + 160 * k));
        bar(f, kBars[k][0], v);
        bar(f, kBars[k][1], v);
      }
      break;
    }
    case Anim::BOOT: {
      f.seg[0][G] = f.seg[3][G] = at(kBootEye, t);
      const uint8_t l = at(kBootLidLeft, t), r = at(kBootLidRight, t);
      f.seg[0][A] = f.seg[0][B] = f.seg[0][F] = l;
      f.seg[3][A] = f.seg[3][B] = f.seg[3][F] = r;
      break;
    }
    case Anim::PAUSE: {
      const uint8_t v = at(kPause, t);
      bar(f, {1, true}, v);
      bar(f, {2, false}, v);
      break;
    }
    case Anim::NEXT:
    case Anim::PREVIOUS: {
      static constexpr Bar kNext[4] = {{2, false}, {2, true}, {3, false}, {3, true}};
      static constexpr Bar kPrev[4] = {{1, true}, {1, false}, {0, true}, {0, false}};
      const Bar *cols = a == Anim::NEXT ? kNext : kPrev;
      for (int c = 0; c < 4; c++) bar(f, cols[c], sweep_column(c, t));
      break;
    }
  }
}

void levels(const std::array<float, 4> &digits, Frame &f) {
  f = {};
  auto lit = [](float v) { return uint8_t(std::lround(255.f * std::clamp(v, 0.f, 1.f))); };
  for (int d = 0; d < 4; d++) {
    const float x = 3 * digits[d];
    f.seg[d][D] = lit(x);
    f.seg[d][G] = lit(x - 1);
    f.seg[d][A] = lit(x - 2);
  }
}

}  // namespace cs::clockfx
