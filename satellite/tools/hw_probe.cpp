// Hardware probe: cycles LED UI states, drives the clock, prints buttons/mute/lux.
// usage: hw_probe [input_seconds=15] [state_ms=2000]
#include <fcntl.h>
#include <unistd.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <string>
#include <vector>

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

static std::string ring_dump() {
  std::string s;
  char b[32];
  for (int i = 0; i < 24; i += 3) {
    snprintf(b, sizeof b, "%d:%02x%02x%02x ", i, led("cucumber:red:ring" + std::to_string(i)),
             led("cucumber:green:ring" + std::to_string(i)), led("cucumber:blue:ring" + std::to_string(i)));
    s += b;
  }
  snprintf(b, sizeof b, "logo0:%02x%02x%02x", led("cucumber:red:logo0"), led("cucumber:green:logo0"),
           led("cucumber:blue:logo0"));
  return s + b;
}

static std::string clock_dump() {
  static const uint8_t font[10] = {0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f};
  std::string s;
  for (int d = 1; d <= 4; d++) {
    uint8_t m = 0;
    for (int seg = 0; seg < 7; seg++)
      if (led(std::string("cucumber:white:clock-") + char('a' + seg) + std::to_string(d)) > 0) m |= 1 << seg;
    char c = '?';
    if (m == 0) c = ' ';
    else if (m == 0x40) c = '-';
    for (int i = 0; i < 10; i++)
      if (font[i] == m) c = char('0' + i);
    s += c;
    if (d == 2) s += led("cucumber:white:clock-p3") ? ':' : ' ';
  }
  return "[" + s + "] current=" + rd("/sys/bus/i2c/devices/3-0037/current") + "/" +
         rd("/sys/bus/i2c/devices/3-0021/current");
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
    const char *name;
    std::function<void()> fn;
  };
  std::vector<Step> steps = {
      {"WAKE", [&] { leds.set_ui_state(UiState::WAKE); }},
      {"LISTENING", [&] { leds.set_ui_state(UiState::LISTENING); }},
      {"THINKING", [&] { leds.set_ui_state(UiState::THINKING); }},
      {"SPEAKING", [&] { leds.set_ui_state(UiState::SPEAKING); }},
      {"ERROR(->IDLE)", [&] { leds.set_ui_state(UiState::ERROR); }},
      {"MUTED", [&] { leds.set_ui_state(UiState::MUTED); }},
      {"TIMER_RING", [&] { leds.set_ui_state(UiState::TIMER_RING); }},
      {"IDLE user solid green 0.5", [&] {
         leds.set_ui_state(UiState::IDLE);
         leds.set_user_light(true, Rgb{0, 255, 0}, 0.5f, "");
       }},
      {"IDLE user Rainbow", [&] { leds.set_user_light(true, Rgb{255, 255, 255}, 0.6f, "Rainbow"); }},
      {"IDLE user Breathe", [&] { leds.set_user_light(true, Rgb{255, 0, 255}, 0.6f, "Breathe"); }},
      {"show_volume 0.5 (over user light off)", [&] {
         leds.set_user_light(false, Rgb{}, 0, "");
         leds.show_volume(0.5f);
       }},
      {"clock flash_number 42", [&] { clock.flash_number(42); }},
      {"clock set_timer 125 s", [&] { clock.set_timer(125000, true); }},
      {"clock timer cleared, brightness 1.0 lux 0", [&] {
         clock.set_timer(0, true);
         clock.set_brightness(1.0f);
         clock.set_ambient_lux(0);
       }},
      {"clock lux 10", [&] { clock.set_ambient_lux(10); }},
      {"clock lux 1000", [&] { clock.set_ambient_lux(1000); }},
      {"clock disabled", [&] { clock.set_enabled(false); }},
      {"clock enabled, idle off", [&] {
         clock.set_enabled(true);
         leds.set_ui_state(UiState::IDLE);
       }},
  };
  for (size_t i = 0; i < steps.size(); i++) {
    loop.add_timer(int64_t(i) * state_ms, [&, i] {
      printf("== %s\n", steps[i].name);
      steps[i].fn();
    });
    loop.add_timer(int64_t(i) * state_ms + state_ms / 2, [&] {
      printf("   ring %s\n   clock %s\n", ring_dump().c_str(), clock_dump().c_str());
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
