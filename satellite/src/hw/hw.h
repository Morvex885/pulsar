// Hardware UI of the supported Yandex Stations (layout per Model): LED ring (+ logo), 7-segment
// clock, touch/mute buttons, ambient light sensor. All via sysfs / evdev; no Yandex code.
#pragma once

#include <array>
#include <cstdint>
#include <functional>
#include <memory>
#include <string>
#include <vector>

#include "core/loop.h"
#include "model.h"

namespace cs {

struct MusicFrame;

struct Rgb {
  uint8_t r = 0, g = 0, b = 0;
};

// Visual states driven by the controller. Animations run on the Loop via timers.
enum class UiState {
  IDLE,        // ring off (or led_show / user light / music), clock shows time
  LISTENING,   // streaming to HA
  THINKING,    // intent processing
  SPEAKING,    // TTS playing
  ERROR,       // pipeline error: runs its 3.5 s to the end, then IDLE
  TIMER_RING,  // timer finished, ringing
};

// Ring content, first active wins: volume overlay > voice state (ERROR / TIMER_RING always,
// LISTENING / THINKING / SPEAKING with voice indication on) > one-shots (timer started, mute) >
// led_show > mute (Lite 2: steady dim red) > user light > music visualization > off.
// The Midi logo shows the voice state, else red while muted, else the ring's average colour.
class Leds {
 public:
  Leds(Loop &loop, const Model &model);
  ~Leds();
  bool open();  // framebuffers / sysfs LEDs of Model::ring and Model::logo
  void set_ui_state(UiState s);
  // Mic mute indication; the first call only sets the state (no one-shot).
  void set_muted(bool muted);
  // User light (HA light entity). Changes fade over transition_ms (0 = cut).
  void set_user_light(bool on, Rgb color, float brightness, const std::string &effect, uint32_t transition_ms);
  // Transient volume indicator (0..1) shown on ring for ~1.5 s.
  void show_volume(float v);
  // Ambient light: dims the ring and logo (smoothed, with hysteresis; floor keeps them visible).
  void set_ambient_lux(float lux);
  void show_timer_started();  // 15 s one-shot
  // Effect (one of effect_names(), "" = solid colour) at full brightness for seconds; seconds <= 0:
  // 5 s, capped at 3600 s. Replaces the previous one.
  void show_temporary(const std::string &effect, Rgb color, int seconds);
  void clear_temporary();
  void set_voice_indication(bool on);
  void set_speed(float k);  // 0.25..4: motion of effects and music styles
  void set_music_style(const std::string &style);  // one of music_styles()
  void set_media_playing(bool playing);
  // Music features, ~30 per second while playing. Sendspin frames take precedence over the speaker
  // reference while they keep coming.
  void music_frame(const MusicFrame &f, bool from_sendspin);
  // Colour correction for the case fabric: a stem of case_colors(), "" = none. False when the file
  // is missing or unreadable (no correction then).
  bool set_case_color(const std::string &stem);
  static const std::vector<std::string> &effect_names();
  static const std::vector<std::string> &music_styles();
  static std::vector<std::string> case_colors();  // sorted stems of /system/vendor/quasar/ledconfigs/*.json

 private:
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

// Clock one-shots (stock patterns, played once): daemon start, player feedback.
enum class ClockShot { BOOT, PAUSE, NEXT, PREVIOUS };

// Clock content, first active wins: pairing code (even when disabled) > volume number > voice
// state (TIMER_RING always, LISTENING / THINKING / SPEAKING with voice indication on) > update
// install (dark) > one-shot > timer countdown > temporary layer > Text / Face > music levels >
// time. Disabled: dark.
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
  // Custom content (Text / Face entities). Text is UTF-8: one cell per character, ':' lights the
  // colon after the preceding character instead of taking a cell; up to 4 cells are shown static
  // and centred, longer text scrolls right-to-left in a loop. set_text and set_face replace each
  // other; set_text("") / set_face(-1) clear only their own kind.
  void set_text(const std::string &utf8);
  void set_face(int index);    // index into face_names(); -1 = none
  void set_scroll_ms(int ms);  // scroll step, clamped to 100..2000
  // Temporary layer above set_text/set_face content. seconds <= 0: one pass of a scrolling text,
  // 5 s otherwise; capped at 3600 s. Replaces the previous layer; "" / -1 removes it.
  void show_temporary_text(const std::string &utf8, int seconds);
  void show_temporary_face(int index, int seconds);
  // Names of set_face indices: the 17 static faces, then "Blink", "All faces".
  static const std::vector<std::string> &face_names();
  void set_ui_state(UiState s);
  void set_voice_indication(bool on);
  void play(ClockShot s);  // restarts / replaces a running one-shot
  void set_updating(bool on);  // dark while an update installs
  void set_music_style(const std::string &style);  // one of music_styles()
  void set_media_playing(bool playing);
  // Music features, ~30 per second while playing; Sendspin frames take precedence over the speaker
  // reference while they keep coming. Shown only while media plays and the style is not "Off".
  void music_frame(const MusicFrame &f, bool from_sendspin);
  static const std::vector<std::string> &music_styles();

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
