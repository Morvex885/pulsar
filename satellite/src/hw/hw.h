// Hardware UI of the supported Yandex Stations (layout per Model): LED ring (+ logo), 7-segment
// clock, touch/mute buttons, ambient light sensor. All via sysfs / evdev; no Yandex code.
#pragma once

#include <array>
#include <cstdint>
#include <functional>
#include <memory>
#include <string>

#include "core/loop.h"
#include "model.h"

namespace cs {

struct Rgb {
  uint8_t r = 0, g = 0, b = 0;
};

// Visual states driven by the controller. Animations run on the Loop via timers.
enum class UiState {
  IDLE,        // ring off (or user light), clock shows time
  WAKE,        // short flash on wake word
  LISTENING,   // streaming to HA
  THINKING,    // intent processing
  SPEAKING,    // TTS playing
  ERROR,       // pipeline error (auto returns to IDLE)
  MUTED,       // microphones muted
  TIMER_RING,  // timer finished, ringing
};

class Leds {
 public:
  Leds(Loop &loop, const Model &model);
  ~Leds();
  bool open();  // framebuffers / sysfs LEDs of Model::ring and Model::logo
  void set_ui_state(UiState s);
  // User light (HA light entity) shown on the ring when UiState is IDLE.
  void set_user_light(bool on, Rgb color, float brightness, const std::string &effect);
  // Transient volume indicator (0..1) shown on ring for ~1.5 s.
  void show_volume(float v);
  // Ambient light: dims the ring and logo (smoothed, with hysteresis; floor keeps them visible).
  void set_ambient_lux(float lux);

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

class Clock {
 public:
  Clock(Loop &loop, const Model &model);
  ~Clock();
  bool open();  // sysfs LEDs <Model::clock_dir><segment><position>
  void set_enabled(bool on);
  void set_brightness(float b);  // 0..1 user scale (multiplied by ambient auto-dimming)
  void set_ambient_lux(float lux);
  // Show a countdown mm:ss (or h:mm) instead of time while ms_left > 0; <= 0 clears. running:
  // counts down on its own from ms_left; otherwise ms_left stays shown (paused timer).
  void set_timer(int64_t ms_left, bool running);
  // Show a number 0..100 briefly (volume feedback).
  void flash_number(int value);
  // Show a pairing code (digits) until clear_code(), overriding everything else, even when the
  // clock is disabled. Codes longer than 4 digits page through 3-digit groups.
  void show_code(const std::string &digits);
  void clear_code();

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

enum class Button { ACTION, VOLUME_UP, VOLUME_DOWN };
enum class Press { SHORT, LONG };

class Input {
 public:
  using ButtonCb = std::function<void(Button, Press)>;
  using MuteCb = std::function<void(bool hw_muted)>;
  Input(Loop &loop, const Model &model);
  ~Input();
  bool open();  // evdev: Model::keys_input (touch keys), Model::mute_input (mic mute switch)
  bool hw_muted() const;
  void on_button(ButtonCb cb);
  void on_mute(MuteCb cb);

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

class LightSensor {
 public:
  using Cb = std::function<void(float lux)>;
  explicit LightSensor(Loop &loop);
  ~LightSensor();
  bool open();  // iio device named "ltr308als01"
  void on_lux(Cb cb, int period_ms = 2000);

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

}  // namespace cs
