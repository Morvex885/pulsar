#include "zigbee/coordinator.h"

#include <algorithm>

#include "core/log.h"
#include "zigbee/bytes.h"

namespace cs {

namespace {

constexpr uint16_t kNetGet = 0x0A00, kNetForm = 0x0A01, kNetErase = 0x0A02, kPermitJoin = 0x0A03, kEndpoint = 0x0A04,
                   kChannel = 0x0A05, kEdScan = 0x0A06, kDataReq = 0x0A10;
constexpr uint16_t kNetState = 0x8A00, kEdScanRsp = 0x8A06, kDataCnf = 0x8A10, kDataInd = 0x8A11, kEvent = 0x8A20;

constexpr uint8_t kStateForming = 1, kStateUp = 2;
constexpr uint8_t kModeGroup = 0x01, kModeNwk = 0x02, kModeIeee = 0x03;
constexpr int64_t kConfirmTimeoutMs = 50'000;  // zigpy gives up on a confirm after 60 s
constexpr int kStateEverySweeps = 6;           // network state (frame counter) every 30 s
constexpr size_t kMaxAsdu = 200;               // the radio's UART buffer holds 256-byte frames

constexpr uint8_t kZdoSuccess = 0x00, kZdoDeviceNotFound = 0x81, kZdoNotActive = 0x83, kZdoNotSupported = 0x84;

}  // namespace

void put_descriptor(std::vector<uint8_t> &v, const SimpleDescriptor &d) {
  v.push_back(d.endpoint);
  put_le16(v, d.profile);
  put_le16(v, d.device);
  v.push_back(d.version);
  v.push_back(uint8_t(d.in.size()));
  for (uint16_t c : d.in) put_le16(v, c);
  v.push_back(uint8_t(d.out.size()));
  for (uint16_t c : d.out) put_le16(v, c);
}

bool get_descriptor(const uint8_t *p, size_t n, SimpleDescriptor &d) {
  if (n < 7) return false;
  d.endpoint = p[0];
  d.profile = get_le16(p + 1);
  d.device = get_le16(p + 3);
  d.version = p[5];
  size_t i = 6;
  for (auto *list : {&d.in, &d.out}) {
    if (i >= n) return false;
    const size_t count = p[i++];
    if (i + 2 * count > n) return false;
    list->clear();
    for (size_t k = 0; k < count; k++, i += 2) list->push_back(get_le16(p + i));
  }
  return true;
}

ZigbeeCoordinator::ZigbeeCoordinator(Loop &loop, TelinkLink &link) : loop_(loop), link_(link) {}

ZigbeeCoordinator::~ZigbeeCoordinator() {
  if (sweep_timer_) loop_.cancel_timer(sweep_timer_);
}

void ZigbeeCoordinator::start(Hooks hooks) {
  hooks_ = std::move(hooks);
  started_ = true;
  link_.on_frame([this](uint16_t type, const uint8_t *p, size_t n) { on_frame(type, p, n); });
  link_.on_boot([this] { on_boot(); });
  sweep_timer_ = loop_.add_timer(5000, [this] { sweep(); }, 5000);
  on_boot();
}

void ZigbeeCoordinator::set_ready(bool r) {
  if (ready_ == r) return;
  ready_ = r;
  if (hooks_.changed) hooks_.changed();
}

void ZigbeeCoordinator::post_sent(uint32_t handle, uint8_t status) {
  loop_.post([this, alive = std::weak_ptr<int>(alive_), handle, status] {
    if (!alive.expired() && hooks_.sent) hooks_.sent(handle, status);
  });
}

void ZigbeeCoordinator::on_boot() {
  // Everything the radio held in RAM is gone, endpoints included.
  for (auto &[h, p] : sent_) post_sent(p.handle, kStatusNoAck);
  sent_.clear();
  if (scan_) post_sent(scan_->handle, kStatusNoAck);
  scan_.reset();
  finish_form(false);
  set_ready(false);
  for (auto &[ep, d] : endpoints_) push_endpoint(d);
  link_.send(kNetGet, {});
}

void ZigbeeCoordinator::push_endpoint(const SimpleDescriptor &d) {
  std::vector<uint8_t> p;
  put_descriptor(p, d);
  link_.send(kEndpoint, std::move(p), [ep = d.endpoint](int st) {
    if (st != 0) LOGW("zigbee: the radio refused endpoint %u (status %d)", ep, st);
  });
}

void ZigbeeCoordinator::set_endpoint(const SimpleDescriptor &d) {
  if (d.endpoint == 0 || d.endpoint == 0xFF) return;
  endpoints_[d.endpoint] = d;
  if (started_) push_endpoint(d);  // else with the others in on_boot
}

void ZigbeeCoordinator::remove_endpoint(uint8_t endpoint) {
  if (!endpoints_.erase(endpoint)) return;
  if (started_) link_.send(kEndpoint, {endpoint, 0xFF});
}

void ZigbeeCoordinator::permit_join(uint8_t seconds) { link_.send(kPermitJoin, {seconds}); }

void ZigbeeCoordinator::factory_reset() {
  LOGW("zigbee: erasing the network in the radio");
  link_.send(kNetErase, {});
}

void ZigbeeCoordinator::form(const NetworkConfig &c, std::function<void(bool ok)> done) {
  LOGI("zigbee: forming a network on channel %u, PAN 0x%04x, update id %u", c.channel, c.pan, c.update_id);
  finish_form(false);  // a previous one still pending is superseded
  form_done_ = std::move(done);
  form_acked_ = false;
  form_config_ = c;
  std::vector<uint8_t> p{c.channel};
  put_le16(p, c.pan);
  put_le64(p, c.ext_pan);
  p.insert(p.end(), c.key.begin(), c.key.end());
  put_le64(p, c.ieee);
  put_le32(p, c.frame_counter);
  p.push_back(c.update_id);
  // Accepted: the state that follows the ACK says whether the formation started.
  link_.send(kNetForm, std::move(p), [this](int st) {
    if (st == 0) {
      form_acked_ = true;
      return;
    }
    LOGW("zigbee: the radio refused to form a network (status %d)", st);
    finish_form(false);
  });
}

void ZigbeeCoordinator::finish_form(bool ok) {
  if (!form_done_) return;
  auto done = std::move(form_done_);
  form_done_ = nullptr;
  done(ok);
}

void ZigbeeCoordinator::change_channel(uint8_t channel, uint8_t update_id) {
  LOGI("zigbee: moving the network to channel %u", channel);
  link_.send(kChannel, {channel, update_id});
}

bool ZigbeeCoordinator::send(uint32_t handle, ApsFrame f, uint64_t dst_ieee) {
  if (!ready_ || !net_.formed || f.data.size() > kMaxAsdu) return false;
  if (!f.group && (dst_ieee ? dst_ieee == net_.ieee : f.dst == net_.nwk)) {
    if (f.profile == 0 && f.dst_ep == 0 && !f.data.empty()) {
      f.dst = net_.nwk;
      local_zdo(handle, f);
    } else {
      post_sent(handle, 0xC1);  // nothing listens on the coordinator's endpoints but the host
    }
    return true;
  }
  if (sent_.count(next_handle_)) post_sent(sent_[next_handle_].handle, kStatusNoAck);
  const uint8_t h = next_handle_++;
  std::vector<uint8_t> p{h};
  if (f.group) {
    p.push_back(kModeGroup);
    put_le16(p, f.dst);
  } else if (dst_ieee) {
    p.push_back(kModeIeee);
    put_le64(p, dst_ieee);
    p.push_back(f.dst_ep);
  } else {
    p.push_back(kModeNwk);
    put_le16(p, f.dst);
    p.push_back(f.dst_ep);
  }
  put_le16(p, f.profile);
  put_le16(p, f.cluster);
  p.push_back(f.src_ep);
  // deCONZ options: 0x01 APS security, 0x02 NWK security (always on here; APS bit 0x02 would turn
  // it off), 0x04 APS ACK, 0x08 fragmentation; the rest are APS tx options as they are.
  p.push_back(f.tx_options & 0x0D);
  p.push_back(f.radius);
  p.push_back(uint8_t(f.data.size()));
  p.insert(p.end(), f.data.begin(), f.data.end());
  sent_[h] = Pending{handle, Loop::now_ms() + kConfirmTimeoutMs};
  link_.send(kDataReq, std::move(p), [this, h](int st) {
    if (st != 0) on_confirm(h, kStatusNoAck);
  });
  return true;
}

void ZigbeeCoordinator::on_confirm(uint8_t radio_handle, uint8_t status) {
  auto it = sent_.find(radio_handle);
  if (it == sent_.end()) return;
  post_sent(it->second.handle, status);
  sent_.erase(it);
}

void ZigbeeCoordinator::deliver_zdo(uint16_t src, uint16_t rsp_cluster, std::vector<uint8_t> payload) {
  ApsFrame f;
  f.src = src;
  f.dst = net_.nwk;
  f.cluster = rsp_cluster;
  f.data = std::move(payload);
  f.lqi = 0xFF;
  loop_.post([this, alive = std::weak_ptr<int>(alive_), f = std::move(f)] {
    if (!alive.expired() && hooks_.received) hooks_.received(f);
  });
}

void ZigbeeCoordinator::local_zdo(uint32_t handle, const ApsFrame &f) {
  const uint8_t tsn = f.data[0];
  const uint8_t *b = f.data.data() + 1;
  const size_t n = f.data.size() - 1;
  const uint16_t self = net_.nwk;
  const uint16_t rsp = f.cluster | 0x8000;
  std::vector<uint8_t> r{tsn, kZdoSuccess};
  auto put_addr = [&] {
    put_le64(r, net_.ieee);
    put_le16(r, self);
  };
  switch (f.cluster) {
    case 0x0000: {  // NWK_addr_req
      if (n >= 8 && get_le64(b) == net_.ieee) {
        put_addr();
      } else {
        r[1] = kZdoDeviceNotFound;
      }
      break;
    }
    case 0x0001:  // IEEE_addr_req
      if (n >= 2 && get_le16(b) == self) {
        put_addr();
      } else {
        r[1] = kZdoDeviceNotFound;
      }
      break;
    case 0x0002:  // Node_Desc_req: coordinator, 2.4 GHz, FFD + mains + RX on + allocates addresses,
                  // manufacturer 0x140A (the radio's own), buffers 82/128, server mask: primary
                  // trust center + network manager, stack compliance revision 22.
      put_le16(r, self);
      r.insert(r.end(), {0x00, 0x40, 0x8F, 0x0A, 0x14, 0x52, 0x80, 0x00, 0x41, 0x2C, 0x80, 0x00, 0x00});
      break;
    case 0x0004: {  // Simple_Desc_req
      put_le16(r, self);
      auto it = n >= 3 ? endpoints_.find(b[2]) : endpoints_.end();
      if (it == endpoints_.end()) {
        r[1] = kZdoNotActive;
        r.push_back(0);
        break;
      }
      std::vector<uint8_t> desc;
      put_descriptor(desc, it->second);
      r.push_back(uint8_t(desc.size()));
      r.insert(r.end(), desc.begin(), desc.end());
      break;
    }
    case 0x0005:  // Active_EP_req
      put_le16(r, self);
      r.push_back(uint8_t(endpoints_.size()));
      for (auto &[ep, d] : endpoints_) r.push_back(ep);
      break;
    case 0x0036:  // Mgmt_Permit_Joining_req
      if (n >= 1) permit_join(b[0]);
      break;
    case 0x0038:  // Mgmt_NWK_Update_req: energy scan of the coordinator itself
      if (n >= 5 && b[4] > 5) {  // channel change / manager update: no response
        post_sent(handle, kZdoSuccess);
        return;
      }
      if (n < 5) {
        r[1] = kZdoNotSupported;
        break;
      }
      energy_scan(handle, tsn, get_le32(b) & 0x07FFF800, b[4]);
      return;
    default:
      r[1] = kZdoNotSupported;
      break;
  }
  post_sent(handle, kZdoSuccess);
  deliver_zdo(self, rsp, std::move(r));
}

void ZigbeeCoordinator::energy_scan(uint32_t handle, uint8_t tsn, uint32_t mask, uint8_t exponent) {
  if (scan_) {
    post_sent(handle, kZdoSuccess);
    deliver_zdo(net_.nwk, 0x8038, {tsn, kZdoNotSupported});
    return;
  }
  scan_ = Scan{handle, tsn, mask};
  std::vector<uint8_t> p;
  put_le32(p, mask);
  p.push_back(exponent);
  const int64_t ms = 1536 * ((1 << exponent) + 1) / 100;  // aBaseSuperframeDuration * (2^n + 1)
  // The radio is off the network channel meanwhile: hold other frames back.
  link_.send(kEdScan, std::move(p), [this](int st) {
    if (st == 0 || !scan_) return;
    post_sent(scan_->handle, kStatusNoAck);
    scan_.reset();
  }, kEdScanRsp, __builtin_popcount(mask) * ms + 3000);
}

void ZigbeeCoordinator::on_scan(const uint8_t *p, size_t n) {
  if (!scan_) return;
  const Scan s = *scan_;
  scan_.reset();
  // [count] count x [channel][energy 0..255], channels ascending as in the mask ->
  // Mgmt_NWK_Update_notify: channels, transmissions, failures, energies.
  std::vector<uint8_t> r{s.tsn, kZdoSuccess};
  put_le32(r, s.mask);
  put_le16(r, 0);
  put_le16(r, 0);
  const size_t count = n >= 1 ? std::min<size_t>(p[0], (n - 1) / 2) : 0;
  r.push_back(uint8_t(count));
  std::string measured;
  for (size_t i = 0; i < count; i++) {
    r.push_back(p[2 + 2 * i]);
    measured += " " + std::to_string(p[1 + 2 * i]) + ":" + std::to_string(p[2 + 2 * i]);
  }
  LOGI("zigbee: energy scan (channel:energy)%s", measured.c_str());
  post_sent(s.handle, kZdoSuccess);
  deliver_zdo(net_.nwk, 0x8038, std::move(r));
}

void ZigbeeCoordinator::on_state(const uint8_t *p, size_t n) {
  // [state][status][channel][PAN][ext PAN 8][IEEE 8][NWK][update id][key 16][key seq][frame counter 4]
  if (n < 45) return;
  ZigbeeNetwork net;
  net.formed = p[0] == kStateUp;
  net.forming = p[0] == kStateForming;
  net.channel = p[2];
  net.pan = get_le16(p + 3);
  net.ext_pan = get_le64(p + 5);
  net.ieee = get_le64(p + 13);
  net.nwk = get_le16(p + 21);
  net.update_id = p[23];
  std::copy(p + 24, p + 40, net.key.begin());
  net.key_seq = p[40];
  net.frame_counter = get_le32(p + 41);
  const bool news = net.formed != net_.formed || net.forming != net_.forming || net.pan != net_.pan ||
                    net.channel != net_.channel || !ready_;
  if (news)
    LOGI("zigbee: radio %s, PAN 0x%04x, channel %u (status %u)",
         net.formed ? "on its network" : net.forming ? "forming a network" : "has no network", net.pan, net.channel,
         p[1]);
  net_ = net;
  if (form_done_ && form_acked_ && !net.forming) {
    if (net.formed && form_config_.pan != 0xFFFF && net.pan != form_config_.pan)
      LOGW("zigbee: PAN ID 0x%04x was taken on channel %u, the network got 0x%04x", form_config_.pan, net.channel,
           net.pan);
    finish_form(net.formed);
  }
  if (!ready_) return set_ready(true);
  if (news && hooks_.changed) hooks_.changed();
}

void ZigbeeCoordinator::on_indication(const uint8_t *p, size_t n) {
  // [dst mode][dst][dst ep][src mode][src 2|8][src ep][profile][cluster][lqi][rssi][security][length][asdu]
  if (n < 5) return;
  ApsFrame f;
  f.group = p[0] == kModeGroup;
  f.dst = get_le16(p + 1);
  f.dst_ep = p[3];
  size_t i = 5;
  if (p[4] == kModeIeee) {
    f.src = 0xFFFE;  // NWK unknown; deCONZ indications carry a NWK source
    i += 8;
  } else {
    if (n < i + 2) return;
    f.src = get_le16(p + i);
    i += 2;
  }
  if (n < i + 9) return;
  f.src_ep = p[i];
  f.profile = get_le16(p + i + 1);
  f.cluster = get_le16(p + i + 3);
  f.lqi = p[i + 5];
  f.rssi = int8_t(p[i + 6]);
  const size_t len = p[i + 8];
  i += 9;
  if (n < i + len) return;
  f.data.assign(p + i, p + i + len);
  if (hooks_.received) hooks_.received(f);
}

void ZigbeeCoordinator::on_event(const uint8_t *p, size_t n) {
  if (n >= 20 && p[0] == 1) {
    LOGI("zigbee: device %016llx joined as 0x%04x (status %u)", (unsigned long long)get_le64(p + 3), get_le16(p + 1),
         p[19]);
  } else if (n >= 10 && p[0] == 2) {
    LOGI("zigbee: device %016llx left%s", (unsigned long long)get_le64(p + 1), p[9] ? " (rejoining)" : "");
  } else if (n >= 2 && p[0] == 3) {
    LOGI("zigbee: joining %s", p[1] ? ("permitted for " + std::to_string(p[1]) + " s").c_str() : "closed");
  }
}

void ZigbeeCoordinator::on_frame(uint16_t type, const uint8_t *p, size_t n) {
  switch (type) {
    case kNetState:
      on_state(p, n);
      break;
    case kDataCnf:
      if (n >= 2) on_confirm(p[0], p[1]);
      break;
    case kDataInd:
      on_indication(p, n);
      break;
    case kEvent:
      on_event(p, n);
      break;
    case kEdScanRsp:
      on_scan(p, n);
      break;
    default:
      break;
  }
}

void ZigbeeCoordinator::sweep() {
  const int64_t now = Loop::now_ms();
  for (auto it = sent_.begin(); it != sent_.end();) {
    if (it->second.deadline > now) {
      ++it;
      continue;
    }
    post_sent(it->second.handle, kStatusNoAck);
    it = sent_.erase(it);
  }
  // The radio reports its state on changes only; the frame counter hosts back up moves on.
  if (++sweeps_ % kStateEverySweeps == 0 && ready_ && !scan_) link_.send(kNetGet, {});
}

}  // namespace cs
