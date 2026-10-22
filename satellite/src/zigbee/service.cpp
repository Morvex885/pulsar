#include "zigbee/service.h"

#include <cstdio>

#include "core/log.h"
#include "zigbee/bytes.h"

namespace cs {

namespace {

constexpr int64_t kAnswerTimeoutMs = 10'000;  // reset + boot + 0x0900 normally take < 1 s
constexpr int64_t kRetryMs = 60'000;          // a silent radio is asked again
constexpr int kMaxFlashes = 2;                 // per start: the image, and one retry
constexpr uint16_t kAppInfoReq = 0x0900, kAppInfoRsp = 0x8900;
constexpr uint8_t kGatewayFamily = 0x15;       // Yandex's gateway.ybl

}  // namespace

const ZigbeeService::Role ZigbeeService::kRoles[3] = {
    {Mode::OFF, "off", "Off", 0},
    {Mode::COORDINATOR, "pulsar-coordinator", "Coordinator", 0x51},
    {Mode::ROUTER, "router", "Router", 0x50},
};

const ZigbeeService::Role &ZigbeeService::role(Mode m) {
  for (const Role &r : kRoles)
    if (r.mode == m) return r;
  return kRoles[0];
}

ZigbeeService::Mode ZigbeeService::parse_mode(const std::string &name) {
  for (const Role &r : kRoles)
    if (name == r.name) return r.mode;
  return Mode::OFF;
}

ZigbeeService::ZigbeeService(Loop &loop, Config cfg, Mode mode, std::function<void()> changed)
    : loop_(loop), cfg_(std::move(cfg)), mode_(mode), changed_(std::move(changed)) {}

const std::string &ZigbeeService::image_path() const {
  return mode_ == Mode::ROUTER ? cfg_.router_image : cfg_.coordinator_image;
}

ZigbeeService::~ZigbeeService() {
  // An upload interrupted here (shutdown watchdog) leaves the radio without an application: the
  // next start finds it silent and writes the image again.
  if (flasher_.joinable()) flasher_.join();
  if (answer_timer_) loop_.cancel_timer(answer_timer_);
  if (retry_timer_) loop_.cancel_timer(retry_timer_);
  tear_down();
}

void ZigbeeService::changed() {
  if (changed_) changed_();
}

void ZigbeeService::start() {
  LOGI("zigbee: role %s", role(mode_).name);
  if (mode_ == Mode::OFF) return changed();
  bring_up();
}

// Opens the radio and asks which firmware runs; the role object comes once it is the right one.
void ZigbeeService::bring_up() {
  phase_ = Phase::STARTING;
  const RadioLines &l = cfg_.lines;
  link_ = std::make_unique<TelinkLink>(loop_, l.tty, l.reset, l.boot);
  link_->on_boot([this] { link_->send(kAppInfoReq, {}); });
  link_->on_frame([this](uint16_t type, const uint8_t *p, size_t n) {
    // [version BE32][manufacturer BE16][image type BE16][size BE32], the same from every firmware
    if (type == kAppInfoRsp && n >= 12) on_firmware(get_be32(p));
  });
  if (!link_->open()) return fail("cannot open the radio");
  if (answer_timer_) loop_.cancel_timer(answer_timer_);
  answer_timer_ = loop_.add_timer(kAnswerTimeoutMs, [this] {
    answer_timer_ = 0;
    if (phase_ != Phase::STARTING) return;
    LOGW("zigbee: the radio does not identify its firmware");
    // Most likely an interrupted upload left no application, which only writing the image fixes.
    flash();
    if (phase_ != Phase::FAILED) return;
    retry_timer_ = loop_.add_timer(kRetryMs, [this] {  // writing refused: maybe it answers later
      retry_timer_ = 0;
      bring_up();
    });
  });
  changed();
}

void ZigbeeService::tear_down() {
  deconz_.reset();  // holds a reference to coord_
  coord_.reset();
  router_.reset();
  link_.reset();
}

void ZigbeeService::on_firmware(uint32_t version) {
  if (phase_ != Phase::STARTING) return;
  if (answer_timer_) loop_.cancel_timer(answer_timer_);
  answer_timer_ = 0;
  // The role's image decides, as maind does it for the gateway: another version of the same
  // firmware is replaced too (a pulsar update brings a new build). Without a readable image only
  // the firmware family is checked.
  const std::string &path = image_path();
  std::vector<uint8_t> image;
  YblInfo want;
  std::string err;
  const bool known = ybl_load(path, &image, &want, &err);
  const uint8_t family = uint8_t(version >> 24);
  const bool same_family = family == role(mode_).family;
  LOGI("zigbee: radio firmware 0x%08x", version);
  // Called from inside the link's frame handler: the role object takes the link over from the loop.
  if (known ? version == want.version : same_family) {
    if (cfg_.outcome) cfg_.outcome(0);
    loop_.post([this] { run(); });
    return;
  }
  if (same_family && want.version == cfg_.failed_version) {  // the radio did not take the new build
    LOGW("zigbee: keeping radio firmware 0x%08x (0x%08x was not taken)", version, want.version);
    loop_.post([this] { run(); });
    return;
  }
  const char *running = family == kGatewayFamily ? "Yandex's gateway"
                        : family == role(Mode::ROUTER).family ? "router"
                        : family == role(Mode::COORDINATOR).family ? "coordinator"
                                                                   : "unknown";
  LOGI("zigbee: radio runs %s firmware, the %s role needs %s 0x%08x", running, role(mode_).name, path.c_str(),
       want.version);
  loop_.post([this] { flash(); });
}

void ZigbeeService::run() {
  if (phase_ != Phase::STARTING || !link_) return;
  phase_ = Phase::RUNNING;
  error_.clear();
  if (mode_ == Mode::ROUTER) {
    router_ = std::make_unique<ZigbeeRouter>(loop_, *link_);
    router_->start({[this] { changed(); }});
    return changed();
  }
  coord_ = std::make_unique<ZigbeeCoordinator>(loop_, *link_);
  deconz_ = std::make_unique<DeconzServer>(loop_, *coord_, cfg_.deconz_port);
  if (!deconz_->start()) return fail("cannot open the deCONZ port");
  ZigbeeCoordinator::Hooks h;
  h.received = [this](const ApsFrame &f) { deconz_->on_received(f); };
  h.sent = [this](uint32_t handle, uint8_t status) { deconz_->on_sent(handle, status); };
  h.changed = [this] {
    deconz_->on_changed();
    changed();
  };
  coord_->start(std::move(h));
  changed();
}

void ZigbeeService::flash() {
  if (phase_ == Phase::FLASHING) return;
  tear_down();
  const std::string &path = image_path();
  auto image = std::make_shared<std::vector<uint8_t>>();
  YblInfo info;
  std::string err;
  if (!ybl_load(path, image.get(), &info, &err)) return fail(err);
  char why[128];
  if (info.version == cfg_.failed_version) {
    // Given up on in an earlier start: not written on every restart. A new image (update) or a
    // role change tries again.
    snprintf(why, sizeof why, "the radio did not take firmware 0x%08x; not written again", info.version);
    return fail(why);
  }
  if (flashes_ >= kMaxFlashes) {
    if (cfg_.outcome) cfg_.outcome(info.version);
    snprintf(why, sizeof why, "the radio does not take firmware 0x%08x", info.version);
    return fail(why);
  }
  flashes_++;
  phase_ = Phase::FLASHING;
  progress_ = 0;
  LOGI("zigbee: writing %s (version 0x%08x, %zu bytes) to the radio", path.c_str(), info.version, image->size());
  changed();
  if (flasher_.joinable()) flasher_.join();
  flasher_ = std::thread([this, image, lines = cfg_.lines] {
    int last = -1;
    std::string ferr;
    const bool ok = radio_flash(lines, *image, [&](float p) {
      const int pct = int(p * 100);
      if (pct / 5 == last / 5) return;
      last = pct;
      loop_.post([this, p] {
        progress_ = p;
        changed();
      });
    }, &ferr);
    loop_.post([this, ok, ferr] {
      if (flasher_.joinable()) flasher_.join();
      if (!ok) {
        error_ = ferr;
        LOGE("zigbee: writing the radio failed: %s", ferr.c_str());
      } else {
        LOGI("zigbee: radio firmware written");
      }
      // Either way the radio runs some application again (or none): identify it.
      bring_up();
    });
  });
}

void ZigbeeService::fail(const std::string &why) {
  LOGE("zigbee: %s", why.c_str());
  tear_down();
  phase_ = Phase::FAILED;
  error_ = why;
  changed();
}

void ZigbeeService::join() {
  if (router_ && phase_ == Phase::RUNNING) router_->join();
}

void ZigbeeService::leave() {
  if (router_ && phase_ == Phase::RUNNING) router_->leave();
}

std::string ZigbeeService::status() const {
  char b[96];
  switch (phase_) {
    case Phase::OFF:
      return "off";
    case Phase::FLASHING:
      snprintf(b, sizeof b, "writing %s firmware: %d %%", role(mode_).label, int(progress_ * 100));
      return b;
    case Phase::FAILED:
      return "error: " + error_;
    case Phase::STARTING:
      return "starting";
    case Phase::RUNNING:
      break;
  }
  if (mode_ == Mode::COORDINATOR) {
    if (!coord_ || !coord_->ready()) return "coordinator: radio not ready";
    const ZigbeeNetwork &n = coord_->network();
    if (n.forming) return "coordinator: forming a network";
    if (!n.formed) return "coordinator: no network";
    snprintf(b, sizeof b, "coordinator: PAN 0x%04X, channel %u", n.pan, n.channel);
    return b;
  }
  const ZigbeeRouter::State &s = router_ ? router_->state() : ZigbeeRouter::State{};
  if (!s.ready) return "router: radio not ready";
  if (s.joining) return "router: joining";
  if (s.joined) {
    snprintf(b, sizeof b, "router: in network 0x%04X as 0x%04X, channel %u", s.pan, s.nwk, s.channel);
    return b;
  }
  return s.error.empty() ? "router: not in a network" : "router: " + s.error;
}

}  // namespace cs
