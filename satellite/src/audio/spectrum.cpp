#include "audio/spectrum.h"

#include <algorithm>
#include <cmath>

namespace cs {

namespace {

constexpr float kPi = 3.14159265358979f;
constexpr float kRangeDb = 45;       // band value 0 this far under the running peak
constexpr float kDecayDb = 0.096f;   // per 32 ms chunk: the peak follows a fade-out at 3 dB/s
constexpr float kFloorDb = -70;
constexpr float kSilenceDb = -75;    // total power under this: nothing is playing
constexpr float kBassAlpha = 0.031f; // ~1 s mean of the bass power
constexpr int kBeatGap = 8;          // chunks (~250 ms) between beats

float to_db(float p) { return 10.f * std::log10(p + 1e-12f); }

}  // namespace

SpectrumAnalyzer::SpectrumAnalyzer(unsigned rate, size_t chunk_frames) {
  reset();
  if (rate == 0 || rate % 16000 != 0 || chunk_frames / (rate / 16000) != kN ||
      chunk_frames % (rate / 16000) != 0)
    return;
  decim_ = rate / 16000;
  for (size_t i = 0; i < kN; i++) window_[i] = 0.5f - 0.5f * std::cos(2 * kPi * i / kN);
  for (size_t k = 0; k < kN / 2; k++) twiddle_[k] = std::polar(1.f, -2 * kPi * k / kN);
  const float bin_hz = 16000.f / kN;
  for (size_t b = 0; b < 8; b++) {
    float lo = 40.f * std::pow(7600.f / 40.f, b / 8.f), hi = 40.f * std::pow(7600.f / 40.f, (b + 1) / 8.f);
    lo_[b] = size_t(std::ceil(lo / bin_hz));
    hi_[b] = std::max(lo_[b], size_t(std::floor(hi / bin_hz)));
  }
}

void SpectrumAnalyzer::reset() {
  ref_db_ = kFloorDb;
  bass_mean_ = 0;
  since_beat_ = 1000;
}

bool SpectrumAnalyzer::process(const float *spk, size_t nspk, MusicFrame &out) {
  if (!decim_ || nspk == 0) return false;
  const float norm = 1.f / float(decim_ * nspk);
  for (size_t i = 0; i < kN; i++) {
    const float *p = spk + i * decim_ * nspk;
    float s = 0;
    for (size_t j = 0; j < decim_ * nspk; j++) s += p[j];
    buf_[i] = {s * norm * window_[i], 0};
  }

  // In-place iterative radix-2 FFT.
  for (size_t i = 1, j = 0; i < kN; i++) {
    size_t bit = kN >> 1;
    for (; j & bit; bit >>= 1) j ^= bit;
    j ^= bit;
    if (i < j) std::swap(buf_[i], buf_[j]);
  }
  for (size_t len = 2; len <= kN; len <<= 1) {
    const size_t step = kN / len;
    for (size_t i = 0; i < kN; i += len)
      for (size_t k = 0; k < len / 2; k++) {
        std::complex<float> u = buf_[i + k], v = buf_[i + k + len / 2] * twiddle_[k * step];
        buf_[i + k] = u + v;
        buf_[i + k + len / 2] = u - v;
      }
  }

  // A full-scale sine through the Hann window peaks at N/4: that is 0 dB.
  const float scale = 16.f / float(kN * kN);
  std::array<float, 8> power{};
  float total = 0, max_db = kFloorDb;
  for (size_t b = 0; b < 8; b++) {
    float p = 0;
    for (size_t k = lo_[b]; k <= hi_[b]; k++) p += std::norm(buf_[k]);
    power[b] = p * scale / float(hi_[b] - lo_[b] + 1);
    total += power[b];
    max_db = std::max(max_db, to_db(power[b]));
  }
  out = MusicFrame{};
  since_beat_++;
  if (to_db(total) < kSilenceDb) {
    bass_mean_ += kBassAlpha * (0 - bass_mean_);
    ref_db_ = std::max(kFloorDb, ref_db_ - kDecayDb);
    return true;
  }
  ref_db_ = std::max({max_db, ref_db_ - kDecayDb, kFloorDb});
  auto value = [&](float p) { return std::clamp((to_db(p) - (ref_db_ - kRangeDb)) / kRangeDb, 0.f, 1.f); };
  for (size_t b = 0; b < 8; b++) out.bands[b] = value(power[b]);
  out.loudness = value(total);

  const float bass = power[0] + power[1];
  if (bass > 1.5f * bass_mean_ && value(bass) > 0.3f && since_beat_ >= kBeatGap) {
    out.beat = true;
    since_beat_ = 0;
  }
  bass_mean_ += kBassAlpha * (bass - bass_mean_);
  return true;
}

}  // namespace cs
