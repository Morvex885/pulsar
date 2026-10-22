// Music features for the LED visualization, from either source: the speaker reference of the
// capture (SpectrumAnalyzer) or the Sendspin visualizer role (Music Assistant).
#pragma once

#include <array>

namespace cs {

struct MusicFrame {
  // 0..1 on a dB scale: 0 = 45 dB under the running peak (speaker reference) / -60 dBFS
  // (Sendspin), 1 = the peak / 0 dBFS. Bands log-spaced 40 Hz .. 7.6 kHz, low to high.
  std::array<float, 8> bands{};
  float loudness = 0;
  bool beat = false;
};

}  // namespace cs
