// Ambient light sensor: iio device "ltr308als01". Lux = in_intensity_input when present, otherwise
// (in_intensity_raw + in_intensity_offset) * in_intensity_scale. Gain/integration defaults are set
// by the firmware (/etc/light-sensor.conf) and left untouched. Polled on the Loop.
#include <dirent.h>

#include <cstdlib>
#include <string>

#include "core/log.h"
#include "hw/hw.h"
#include "hw/sysfs.h"

namespace cs {

namespace {
constexpr const char *kIioDir = "/sys/bus/iio/devices/";
constexpr const char *kSensorName = "ltr308als01";
}  // namespace

struct LightSensor::Impl {
  explicit Impl(Loop &l) : loop(l) {}
  Loop &loop;
  std::string dir;
  Cb cb;
  uint64_t timer = 0;

  ~Impl() {
    if (timer) loop.cancel_timer(timer);
  }

  bool read_num(const char *attr, double &out) const {
    std::string s;
    if (!hwutil::read_file(dir + attr, s) || s.empty()) return false;
    char *end = nullptr;
    out = strtod(s.c_str(), &end);
    return end != s.c_str();
  }

  bool read_lux(float &lux) const {
    double v;
    if (read_num("in_intensity_input", v)) {
      lux = float(v);
      return true;
    }
    double raw, scale = 1, offset = 0;
    if (!read_num("in_intensity_raw", raw)) return false;
    read_num("in_intensity_scale", scale);
    read_num("in_intensity_offset", offset);
    lux = float(std::max(0.0, (raw + offset) * scale));
    return true;
  }
};

LightSensor::LightSensor(Loop &loop) : impl_(new Impl(loop)) {}
LightSensor::~LightSensor() = default;

bool LightSensor::open() {
  DIR *d = opendir(kIioDir);
  if (!d) return false;
  while (dirent *e = readdir(d)) {
    if (e->d_name[0] == '.') continue;
    std::string base = std::string(kIioDir) + e->d_name + "/", name;
    if (hwutil::read_file(base + "name", name) && hwutil::trim(name) == kSensorName) {
      impl_->dir = base;
      break;
    }
  }
  closedir(d);
  if (impl_->dir.empty()) {
    LOGE("light: iio device %s not found", kSensorName);
    return false;
  }
  float lux;
  if (!impl_->read_lux(lux)) {
    LOGE("light: cannot read %s", impl_->dir.c_str());
    return false;
  }
  LOGI("light: %s (%.2f lx)", impl_->dir.c_str(), lux);
  return true;
}

void LightSensor::on_lux(Cb cb, int period_ms) {
  auto &d = *impl_;
  d.cb = std::move(cb);
  if (d.timer) d.loop.cancel_timer(d.timer);
  d.timer = 0;
  if (!d.cb || d.dir.empty()) return;
  auto poll = [&d] {
    float lux;
    if (d.read_lux(lux) && d.cb) d.cb(lux);
  };
  poll();
  d.timer = d.loop.add_timer(period_ms, poll, period_ms);
}

}  // namespace cs
