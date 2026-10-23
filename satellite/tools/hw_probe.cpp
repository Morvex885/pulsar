// Hardware probe: cycles LED UI states, effects and music styles, drives the clock and its
// animations, prints buttons/mute/lux.
// usage: hw_probe [input_seconds=15] [state_ms=2000]
#include <fcntl.h>
#include <unistd.h>

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <string>
#include <vector>

#include "audio/music_frame.h"
#include "core/log.h"
#include "core/loop.h"
#include "hw/hw.h"
#include "model.h"

using namespace cs;

static std::string rd(const std::string &p) {
  char b[64] = {};
  int fd = open(p.c_str(), O_RDONLY);
  if (fd < 0) return "?";
  ssize_t n = read(fd, b, sizeof b - 1);
  close(fd);
  if (n <= 0) return "?";
  b[n] = 0;
  if (b[n - 1] == '\n') b[n - 1] = 0;
  return b;
}

static int led(const std::string &name) { return atoi(rd("/sys/class/leds/" + name + "/brightness").c_str()); }

static const Model *g_model;

// Every LED of a short ring, every third of the Midi's; the first logo LED.
static std::string ring_dump() {
  const Model &m = *g_model;
  auto px = [&](const Model::LedStrip &s, int i) {
    auto name = [&](const char *c) {
      return m.led_prefix + ":" + c + ":" + s.sysfs_kind + std::to_string(s.first_index + i);
    };
    char b[16];
    snprintf(b, sizeof b, "%02x%02x%02x", led(name("red")), led(name("green")), led(name("blue")));
    return std::string(b);
  };
  std::string s;
  const int step = m.ring.count > 6 ? 3 : 1;
  for (int i = 0; i < m.ring.count; i += step) s += std::to_string(i) + ":" + px(m.ring, i) + " ";
  if (m.logo.count) s += "logo0:" + px(m.logo, 0);
  return s;
}

static std::string clock_dump() {
  static const uint8_t font[10] = {0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f};
  const Model &m = *g_model;
  auto lit = [&](const std::string &name) { return atoi(rd(m.clock_dir + name + "/brightness").c_str()) > 0; };
  std::string s;
  for (int d = 0; d < 4; d++) {
    uint8_t mask = 0;
    for (int seg = 0; seg < 7; seg++)
      if (lit(std::string(1, char(m.clock_seg_base + seg)) + std::to_string(m.clock_pos_base + d))) mask |= 1 << seg;
    char c = '?';
    if (mask == 0) c = ' ';
    else if (mask == 0x40) c = '-';
    for (int i = 0; i < 10; i++)
      if (font[i] == mask) c = char('0' + i);
    s += c;
    if (d == 1) s += !m.clock_colon.empty() && lit(m.clock_colon[0]) ? ':' : ' ';
  }
  if (!m.clock_chip_current) return "[" + s + "]";
  return "[" + s + "] current=" + rd("/sys/bus/i2c/devices/3-0037/current") + "/" +
         rd("/sys/bus/i2c/devices/3-0021/current");
}

// Brightness of every segment (a..g per digit, 2 hex digits each) and the centre LED.
static std::string clock_values() {
  const Model &m = *g_model;
  auto val = [&](const std::string &name) {
    return std::clamp(atoi(rd(m.clock_dir + name + "/brightness").c_str()), 0, 255);
  };
  std::string s;
  char b[8];
  for (int d = 0; d < 4; d++) {
    for (int seg = 0; seg < 7; seg++) {
      const std::string name = std::string(1, char(m.clock_seg_base + seg)) + std::to_string(m.clock_pos_base + d);
      snprintf(b, sizeof b, "%02x", val(name));
      s += b;
    }
    s += ' ';
  }
  if (!m.clock_center.empty()) {
    snprintf(b, sizeof b, "c=%02x", val(m.clock_center));
    s += b;
  }
  return s;
}

int main(int argc, char **argv) {
  int input_s = argc > 1 ? atoi(argv[1]) : 15;
  int state_ms = argc > 2 ? atoi(argv[2]) : 2000;
  std::string cur37 = rd("/sys/bus/i2c/devices/3-0037/current"), cur21 = rd("/sys/bus/i2c/devices/3-0021/current");

  // Raw framebuffer write rate (ring, 72 bytes).
  {
    int fd = open("/dev/fb0", O_RDWR);
    if (fd >= 0) {
      uint8_t z[72] = {};
      int64_t t0 = Loop::now_ms();
      for (int i = 0; i < 100; i++) (void)!pwrite(fd, z, sizeof z, 0);
      printf("fb0: 100 frame writes in %lld ms\n", (long long)(Loop::now_ms() - t0));
      close(fd);
    }
  }

  const Model *model = detect_model();
  if (!model) return 1;
  g_model = model;
  Loop loop;
  Leds leds(loop, *model);
  Clock clock(loop, *model);
  Input input(loop, *model);
  LightSensor light(loop);
  printf("open: leds=%d clock=%d input=%d light=%d hw_muted=%d\n", leds.open(), clock.open(), input.open(),
         light.open(), input.hw_muted());
  input.on_button([](Button b, Press p) {
    static const char *bn[] = {"ACTION", "VOLUME_UP", "VOLUME_DOWN"};
    printf("BUTTON %s %s\n", bn[int(b)], p == Press::LONG ? "LONG" : "SHORT");
    fflush(stdout);
  });
  input.on_mute([](bool m) {
    printf("MUTE %s\n", m ? "on" : "off");
    fflush(stdout);
  });

  struct Step {
    std::string name;
    std::function<void()> fn;
  };
  // Synthetic music features at 30 frames/s while the music steps run.
  uint64_t music_timer = 0;
  const int64_t music_t0 = Loop::now_ms();
  auto feed_music = [&] {
    if (music_timer) return;
    music_timer = loop.add_timer(33, [&] {
      const double t = double(Loop::now_ms() - music_t0);
      MusicFrame f;
      for (size_t b = 0; b < 8; b++) f.bands[b] = float(0.5 + 0.5 * std::sin(2 * M_PI * (t / 800 + b / 8.0)));
      f.loudness = float(0.5 + 0.5 * std::sin(2 * M_PI * t / 1000));
      static int64_t last_beat = 0;
      if (Loop::now_ms() - last_beat >= 500) f.beat = true, last_beat = Loop::now_ms();
      leds.music_frame(f, false);
      clock.music_frame(f, false);
    }, 33);
  };

  leds.set_muted(false);  // initial state, no indication
  std::vector<Step> steps = {
      {"LISTENING", [&] { leds.set_ui_state(UiState::LISTENING); }},
      {"THINKING", [&] { leds.set_ui_state(UiState::THINKING); }},
      {"SPEAKING", [&] { leds.set_ui_state(UiState::SPEAKING); }},
      {"ERROR", [&] { leds.set_ui_state(UiState::ERROR); }},
      // IDLE during the error is ignored: it runs its 3.54 s to the end, then the ring is off.
      {"IDLE requested during ERROR (off once it ends)", [&] { leds.set_ui_state(UiState::IDLE); }},
      {"TIMER_RING", [&] { leds.set_ui_state(UiState::TIMER_RING); }},
      {"IDLE, mic muted", [&] {
         leds.set_ui_state(UiState::IDLE);
         leds.set_muted(true);
       }},
      {"mic unmuted", [&] { leds.set_muted(false); }},
  };
  for (const auto &e : Leds::effect_names()) {
    static const std::vector<std::string> kWhite = {"Rainbow", "Color loop", "Fire", "Aurora",
                                                    "Lava lamp", "Candle", "Night light"};
    const bool white = std::find(kWhite.begin(), kWhite.end(), e) != kWhite.end();
    const Rgb col = white ? Rgb{255, 255, 255} : Rgb{255, 0, 255};
    steps.push_back({"IDLE user " + e, [&, e, col] {
                       leds.set_ui_state(UiState::IDLE);
                       leds.set_user_light(true, col, 0.6f, e, 0);
                     }});
  }
  steps.push_back({"led_show Comet green 3 s", [&] { leds.show_temporary("Comet", Rgb{0, 255, 0}, 3); }});
  steps.push_back({"show_volume 0.2 (user light off)", [&] {
                     leds.clear_temporary();
                     leds.set_user_light(false, Rgb{}, 0, "", 0);
                     leds.show_volume(0.2f);
                   }});
  steps.push_back({"show_volume 0.6", [&] { leds.show_volume(0.6f); }});
  steps.push_back({"show_volume 1.0", [&] { leds.show_volume(1.0f); }});
  for (const char *style : {"Spectrum", "Pulse", "Disco", "Polar"})
    steps.push_back({std::string("music ") + style, [&, style] {
                       feed_music();
                       leds.set_media_playing(true);
                       leds.set_music_style(style);
                     }});
  const auto cases = Leds::case_colors();
  if (!cases.empty())
    steps.push_back({"solid white, case colour " + cases[0], [&, c = cases[0]] {
                       leds.set_media_playing(false);
                       leds.set_user_light(true, Rgb{255, 255, 255}, 0.6f, "", 0);
                       leds.set_case_color(c);
                     }});
  steps.push_back({"music off, light off, no case colour", [&] {
                     if (music_timer) loop.cancel_timer(music_timer), music_timer = 0;
                     leds.set_media_playing(false);
                     leds.set_user_light(false, Rgb{}, 0, "", 0);
                     leds.set_case_color("");
                   }});
  // Shown above the lamp and music for 15 s: last of the LED steps.
  steps.push_back({"timer started (15 s)", [&] { leds.show_timer_started(); }});
  // The 600 ms mute sweep shows over the countdown, which then goes on (green at the dumps).
  steps.push_back({"mic muted during the countdown", [&] { leds.set_muted(true); }});
  steps.push_back({"mic unmuted during the countdown", [&] { leds.set_muted(false); }});
  const std::vector<Step> clock_steps = {
      {"clock flash_number 42", [&] { clock.flash_number(42); }},
      {"clock set_timer 125 s", [&] { clock.set_timer(125000, true); }},
      {"clock timer cleared, brightness 1.0 lux 0", [&] {
         clock.set_timer(0, true);
         clock.set_brightness(1.0f);
         clock.set_ambient_lux(0);
       }},
      {"clock lux 10", [&] { clock.set_ambient_lux(10); }},
      {"clock lux 1000", [&] { clock.set_ambient_lux(1000); }},
      {"clock LISTENING", [&] { clock.set_ui_state(UiState::LISTENING); }},
      {"clock THINKING", [&] { clock.set_ui_state(UiState::THINKING); }},
      {"clock SPEAKING", [&] { clock.set_ui_state(UiState::SPEAKING); }},
      {"clock voice indication off", [&] { clock.set_voice_indication(false); }},
      {"clock TIMER_RING", [&] { clock.set_ui_state(UiState::TIMER_RING); }},
      {"clock IDLE", [&] {
         clock.set_voice_indication(true);
         clock.set_ui_state(UiState::IDLE);
       }},
      {"clock BOOT", [&] { clock.play(ClockShot::BOOT); }},
      {"clock PAUSE", [&] { clock.play(ClockShot::PAUSE); }},
      {"clock NEXT", [&] { clock.play(ClockShot::NEXT); }},
      {"clock PREVIOUS", [&] { clock.play(ClockShot::PREVIOUS); }},
      {"clock updating", [&] { clock.set_updating(true); }},
      {"clock update done", [&] { clock.set_updating(false); }},
      {"clock music Levels", [&] {
         feed_music();
         clock.set_media_playing(true);
         clock.set_music_style("Levels");
       }},
      {"clock music Off", [&] {
         if (music_timer) loop.cancel_timer(music_timer), music_timer = 0;
         clock.set_music_style("Off");
         clock.set_media_playing(false);
       }},
      {"clock disabled", [&] { clock.set_enabled(false); }},
      {"clock enabled, idle off", [&] {
         clock.set_enabled(true);
         leds.set_ui_state(UiState::IDLE);
       }},
  };
  steps.insert(steps.end(), clock_steps.begin(), clock_steps.end());
  for (size_t i = 0; i < steps.size(); i++) {
    loop.add_timer(int64_t(i) * state_ms, [&, i] {
      printf("== %s\n", steps[i].name.c_str());
      steps[i].fn();
    });
    loop.add_timer(int64_t(i) * state_ms + state_ms / 2, [&] {
      printf("   ring %s\n   clock %s\n   values %s\n", ring_dump().c_str(), clock_dump().c_str(),
             clock_values().c_str());
      fflush(stdout);
    });
    loop.add_timer(int64_t(i) * state_ms + state_ms * 3 / 4, [&] {
      printf("   ring %s\n   values %s\n", ring_dump().c_str(), clock_values().c_str());
      fflush(stdout);
    });
  }
  int64_t t_input = int64_t(steps.size()) * state_ms;
  loop.add_timer(t_input, [&] {
    printf("== monitoring input/lux for %d s (press buttons, toggle mute)\n", input_s);
    fflush(stdout);
    light.on_lux([](float lux) {
      printf("lux %.2f\n", lux);
      fflush(stdout);
    }, 1000);
  });
  loop.add_timer(t_input + input_s * 1000, [&] {
    printf("final clock %s\n", clock_dump().c_str());
    loop.stop();
  });
  loop.run();
  // Restore: ring/logo off (already IDLE/off), original clock current.
  int fd = open("/sys/bus/i2c/devices/3-0037/current", O_WRONLY);
  if (fd >= 0) {
    (void)!write(fd, cur37.c_str(), cur37.size());
    close(fd);
  }
  fd = open("/sys/bus/i2c/devices/3-0021/current", O_WRONLY);
  if (fd >= 0) {
    (void)!write(fd, cur21.c_str(), cur21.size());
    close(fd);
  }
  printf("restored clock current %s/%s\n", cur37.c_str(), cur21.c_str());
  return 0;
}
