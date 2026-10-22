#include "zigbee/router.h"

#include "core/log.h"
#include "zigbee/bytes.h"

namespace cs {

namespace {

constexpr uint16_t kSteer = 0x0002, kFactoryNew = 0x0005, kNwkInfoReq = 0x0045;
constexpr uint16_t kNwkInfoRsp = 0x8045, kJoined = 0x8906, kEvent = 0x8920;
constexpr uint8_t kEventStarted = 1, kEventJoinFailed = 2, kEventLeft = 3;
constexpr int64_t kJoinTimeoutMs = 90'000;  // steering scans every channel, then retries

std::string join_error(uint8_t bdb_status) {
  switch (bdb_status) {
    case 3: return "no open network found (allow joining on the coordinator)";
    case 10: return "trust center key exchange failed";
    default: return "join failed (BDB status " + std::to_string(bdb_status) + ")";
  }
}

}  // namespace

ZigbeeRouter::ZigbeeRouter(Loop &loop, TelinkLink &link) : loop_(loop), link_(link) {}

ZigbeeRouter::~ZigbeeRouter() {
  if (join_timer_) loop_.cancel_timer(join_timer_);
}

void ZigbeeRouter::start(Hooks hooks) {
  hooks_ = std::move(hooks);
  link_.on_frame([this](uint16_t type, const uint8_t *p, size_t n) { on_frame(type, p, n); });
  link_.on_boot([this] { on_boot(); });
  on_boot();
}

void ZigbeeRouter::on_boot() {
  st_.ready = false;
  link_.send(kNwkInfoReq, {});
}

void ZigbeeRouter::changed() {
  if (hooks_.changed) hooks_.changed();
}

void ZigbeeRouter::join() {
  if (!st_.ready || st_.joined || st_.joining) return;
  LOGI("zigbee router: joining");
  st_.joining = true;
  st_.error.clear();
  link_.send(kSteer, {}, [this](int status) {
    if (status == 0) return;
    st_.joining = false;
    st_.error = "radio refused the join command";
    changed();
  });
  if (join_timer_) loop_.cancel_timer(join_timer_);
  join_timer_ = loop_.add_timer(kJoinTimeoutMs, [this] {
    join_timer_ = 0;
    if (!st_.joining) return;
    st_.joining = false;
    st_.error = "no network joined";
    changed();
  });
  changed();
}

void ZigbeeRouter::leave() {
  if (!st_.ready) return;
  LOGI("zigbee router: leaving the network");
  st_.joining = false;
  st_.error.clear();
  // The firmware broadcasts a Leave, erases its network and may restart; ask once it settled.
  link_.send(kFactoryNew, {}, [this](int) {
    loop_.add_timer(1000, [this, alive = std::weak_ptr<int>(alive_)] {
      if (!alive.expired()) link_.send(kNwkInfoReq, {});
    });
  });
}

void ZigbeeRouter::on_frame(uint16_t type, const uint8_t *p, size_t n) {
  switch (type) {
    case kNwkInfoRsp:  // [type][capability][on a network][PAN BE][ext PAN 8][NWK BE][IEEE 8][channel]
    case kJoined:      // same, without the channel
      if (n < 23) break;
      st_.ready = true;
      st_.joined = p[2] != 0;
      st_.pan = get_be16(p + 3);
      st_.nwk = get_be16(p + 13);
      if (type == kNwkInfoRsp && n >= 24) st_.channel = p[23];
      if (st_.joined) {
        st_.joining = false;
        st_.error.clear();
      }
      if (type == kJoined) {
        LOGI("zigbee router: joined PAN 0x%04x as 0x%04x", st_.pan, st_.nwk);
        link_.send(kNwkInfoReq, {});  // the channel; its reply publishes the new state
        break;
      }
      changed();
      break;
    case kEvent:
      if (n < 2) break;
      if (p[0] == kEventJoinFailed) {
        st_.joining = false;
        st_.error = join_error(p[1]);
        LOGW("zigbee router: %s", st_.error.c_str());
      } else if (p[0] == kEventLeft) {
        LOGI("zigbee router: left the network");
        st_.joined = false;
      } else if (p[0] == kEventStarted) {
        link_.send(kNwkInfoReq, {});
      }
      changed();
      break;
    default:
      break;
  }
}

}  // namespace cs
