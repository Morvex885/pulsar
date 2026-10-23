// Music features for the LED visualization, from either source: the speaker reference of the
// capture (SpectrumAnalyzer) or the Sendspin visualizer role (Music Assistant).
#pragma once

#include <array>
#include <cstdint>

namespace cs {

struct MusicFrame {
  // 0..1 on a dB scale: 0 = 45 dB under the running peak (speaker reference) / -60 dBFS
  // (Sendspin), 1 = the peak / 0 dBFS. Bands log-spaced 40 Hz .. 7.6 kHz, low to high.
  std::array<float, 8> bands{};
  float loudness = 0;
  bool beat = false;
};

// Source precedence, shared by the ring and the clock: Sendspin frames win while they keep coming;
// the speaker reference takes over kSendspinFreshMs after the last one. Sendspin pauses up to
// ~0.4 s inside a stream (seen on a Midi, no stream event), and restarts on every seek / skip
// with ~0.7 s without frames. Frames older than kMusicStaleMs count as none; it must exceed
// kSendspinFreshMs by a frame or the display drops out before the speaker reference arrives.
constexpr int64_t kSendspinFreshMs = 500;
constexpr int64_t kMusicStaleMs = 600;

}  // namespace cs
