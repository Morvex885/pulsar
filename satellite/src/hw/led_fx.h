// Pure rendering of the LED ring / logo content: voice states, one-shots, effects and music
// styles. Colours are "perceptual" 0..1 values; Leds squares them (gamma 2) on output.
//
// Strips of 12 LEDs or more are drawn like the Midi's 24-LED contour (LED 0 = home, LED n/2
// opposite, symmetry points 0, n/4, n/2, 3n/4); shorter ones like the Lite 2's 3-LED ring. The
// voice-state colours and timings are the ones measured on the stock patterns
// (/system/vendor/quasar/ledpatterns/{ring,oknix}/*.led); nothing of those files is used at run time.
#pragma once

#include <array>
#include <cstdint>
#include <string>
#include <vector>

namespace cs::fx {

struct Col {
  float r = 0, g = 0, b = 0;
};

inline Col operator*(Col c, float k) { return {c.r * k, c.g * k, c.b * k}; }
inline Col operator+(Col a, Col b) { return {a.r + b.r, a.g + b.g, a.b + b.b}; }
Col rgb(int r, int g, int b);
Col lerp(Col a, Col b, float t);
Col cmax(Col a, Col b);
Col hsv(float h, float s, float v);  // h in turns
// Stock raw value (0xRRGGBB as written to the LEDs) at raw ratio k, in perceptual units: after the
// gamma-2 output it is the stock value times k.
Col stock(uint32_t rgb, float k = 1);

float clamp01(float v);
float smoothstep(float e0, float e1, float x);
// Smooth 0..1 oscillation with the given period, starting at minimum.
float wave(double t, double period);
// 2-D value noise in 0..1; y is the (long-running) time axis.
float noise(float x, double y);
// Rings of 12 LEDs or more are drawn like the Midi's contour, shorter ones like the Lite 2's ring.
inline bool wide(int n) { return n >= 12; }

// Voice states (UiState minus IDLE); t in ms since the state started.
enum class Voice { LISTENING, THINKING, SPEAKING, ERROR, TIMER_RING };
constexpr int64_t kErrorMs = 3540;
void voice(Voice v, int64_t t, Col *out, int n);
void voice_logo(Voice v, int64_t t, Col *out, int n);

constexpr int64_t kTimerStartedMs = 15000;
constexpr int64_t kMuteSweepMs = 600;
void timer_started(int64_t t, Col *out, int n);
// Mute indication one-shot: mic muted (on) / unmuted.
void mute_sweep(bool on, int64_t t, Col *out, int n);
// Steady mute indication of short rings (the Midi shows it on the logo).
Col mute_steady();
Col mute_logo();
void volume(float v, Col *out, int n);

// Per-layer state of the stateful effects (Twinkle).
struct EffectState {
  uint32_t rng = 0x9E3779B9u;
  std::vector<float> age;  // per LED, seconds since its sparkle started; < 0 idle
};
// t: effect phase in s (speed-scaled), dt: its step since the previous frame. bri scales the
// result. Returns true if the content moves (needs further frames).
bool effect(const std::string &name, Col c, float bri, double t, double dt, EffectState &st, Col *out, int n);

// Music styles, without "Off" / "All styles" (resolved by Leds).
enum class Style { SPECTRUM, PULSE, DISCO, POLAR };
struct MusicInput {
  std::array<float, 8> bands{};
  float loudness = 0;
  float flash = 0;  // 1 on a beat, decaying
};
struct MusicState {
  double theta = 0;    // Disco rotation, turns
  double impulse = 0;  // Disco beat impulse, turns/s
  double p = 0, target = 0;  // Polar centre / target, turns
  double next_pick = 0;      // Polar: phase of the next target pick
  uint32_t rng = 0x2545F491u;
};
// Advances the moving styles by phase step dt (speed-scaled seconds) at phase t.
void music_step(MusicState &st, const MusicInput &in, bool beat, double t, double dt);
void music(Style s, const MusicState &st, const MusicInput &in, double t, Col *out, int n);

}  // namespace cs::fx
