#include "zigbee/deconz_server.h"

#include <arpa/inet.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <cstring>

#include "core/log.h"
#include "zigbee/bytes.h"

namespace cs {

namespace {

constexpr uint8_t kEnd = 0xC0, kEsc = 0xDB, kEscEnd = 0xDC, kEscEsc = 0xDD;

// Commands.
constexpr uint8_t kApsDataConfirm = 0x04, kDeviceState = 0x07, kChangeNetworkState = 0x08, kReadParameter = 0x0A,
                  kWriteParameter = 0x0B, kVersion = 0x0D, kDeviceStateChanged = 0x0E, kApsDataRequest = 0x12,
                  kApsDataIndication = 0x17;
// Status.
constexpr uint8_t kSuccess = 0, kFailure = 1, kUnsupported = 4, kInvalidValue = 7;
// Network states and device state flags (in the device state byte, above the 2-bit state).
constexpr uint8_t kOffline = 0, kJoining = 1, kConnected = 2;
constexpr uint8_t kFlagConfirm = 0x04, kFlagIndication = 0x08, kFlagFreeSlots = 0x20;
// Parameters.
constexpr uint8_t kMacAddress = 0x01, kNwkPanId = 0x05, kNwkAddress = 0x07, kNwkExtPanId = 0x08,
                  kDesignedCoordinator = 0x09, kChannelMask = 0x0A, kApsExtPanId = 0x0B, kTrustCenterAddress = 0x0E,
                  kSecurityMode = 0x10, kConfigureEndpoint = 0x13, kPredefinedPanId = 0x15, kNetworkKey = 0x18,
                  kLinkKey = 0x19, kCurrentChannel = 0x1C, kPermitJoin = 0x21, kProtocolVersion = 0x22,
                  kNwkUpdateId = 0x24, kWatchdogTtl = 0x26, kFrameCounter = 0x27, kZdpResponseHandling = 0x28;

// ConBee III (platform 0x09) firmware 0x26780900: zigpy-deconz then scans energy with ZDO
// Mgmt_NWK_Update_req to the coordinator (answered by ZigbeeCoordinator) and takes the values as
// they are. Protocol version 0x0106 keeps zigpy off the commands this radio has no equivalent for
// (update_neighbor, watchdog TTL, source routes).
constexpr uint32_t kFirmwareVersion = 0x26780900;
constexpr uint16_t kProtocol = 0x0106;
constexpr size_t kMaxQueued = 64;
constexpr size_t kMaxInFlight = 8;  // sends awaiting confirm before "no free slots"
constexpr uint8_t kSecurityOnlyTclk = 0x03;
const uint8_t kTcLinkKey[16] = {'Z', 'i', 'g', 'B', 'e', 'e', 'A', 'l', 'l', 'i', 'a', 'n', 'c', 'e', '0', '9'};

}  // namespace

DeconzServer::DeconzServer(Loop &loop, ZigbeeCoordinator &coordinator, uint16_t port)
    : loop_(loop), coord_(coordinator), port_(port) {
  // A ConBee's slots, until the host writes its own.
  set_slot(0, SimpleDescriptor{1, 0x0104, 0x0005, 0, {0x0000}, {}});          // HA
  set_slot(1, SimpleDescriptor{2, 0xC05E, 0x0840, 0, {0x0000}, {}});          // ZLL
  set_slot(2, SimpleDescriptor{0xF2, 0xA1E0, 0x0061, 0, {0x0021}, {0x0021}});  // Green Power
}

void DeconzServer::set_slot(size_t slot, const SimpleDescriptor &d) {
  // A slot taking another endpoint number drops the old endpoint unless another slot holds it
  // (zigbee-herdsman writes endpoint 0xF2 into slot 1).
  if (slots_[slot] && slots_[slot]->endpoint != d.endpoint) {
    const uint8_t old = slots_[slot]->endpoint;
    slots_[slot].reset();
    bool held = false;
    for (auto &s : slots_) held |= s && s->endpoint == old;
    if (!held) coord_.remove_endpoint(old);
  }
  slots_[slot] = d;
  coord_.set_endpoint(d);
}

DeconzServer::~DeconzServer() {
  close_client();
  if (listen_fd_ >= 0) {
    loop_.del_fd(listen_fd_);
    ::close(listen_fd_);
  }
}

bool DeconzServer::start() {
  listen_fd_ = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
  if (listen_fd_ < 0) return false;
  int one = 1;
  setsockopt(listen_fd_, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
  sockaddr_in sa{};
  sa.sin_family = AF_INET;
  sa.sin_port = htons(port_);
  sa.sin_addr.s_addr = htonl(INADDR_ANY);
  if (bind(listen_fd_, reinterpret_cast<sockaddr *>(&sa), sizeof sa) != 0 || listen(listen_fd_, 2) != 0) {
    LOGE("zigbee: bind/listen :%u: %s", port_, strerror(errno));
    ::close(listen_fd_);
    listen_fd_ = -1;
    return false;
  }
  loop_.add_fd(listen_fd_, EPOLLIN, [this](uint32_t) { on_accept(); });
  LOGI("zigbee: deCONZ coordinator on :%u", port_);
  return true;
}

void DeconzServer::on_accept() {
  sockaddr_in sa{};
  socklen_t sl = sizeof sa;
  int fd = accept4(listen_fd_, reinterpret_cast<sockaddr *>(&sa), &sl, SOCK_NONBLOCK | SOCK_CLOEXEC);
  if (fd < 0) return;
  // Until the radio has reported its network (start-up, or after it restarted), no host is served:
  // one told "no network" would form one, which erases the radio. zigpy and herdsman retry a
  // refused connection; error statuses would break zigpy-deconz (it asserts UNSUPPORTED).
  if (!coord_.ready()) {
    ::close(fd);
    return;
  }
  if (client_fd_ >= 0) {
    LOGI("zigbee: new client replaces the previous one");
    close_client();
  }
  int one = 1;
  setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
  setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, &one, sizeof one);
  char ip[INET_ADDRSTRLEN] = "";
  inet_ntop(AF_INET, &sa.sin_addr, ip, sizeof ip);
  LOGI("zigbee: client %s connected", ip);
  client_fd_ = fd;
  generation_++;
  online_ = true;
  // Network parameters written by a previous client are not this one's (a formation it started
  // goes on with pending_).
  new_network_ = false;
  pan_.reset();
  ext_pan_.reset();
  ieee_.reset();
  key_.reset();
  frame_counter_ = 0;
  channel_mask_ = 0;
  update_id_ = 0;
  last_state_ = 0xFF;
  in_.clear();
  out_.clear();
  indications_.clear();
  confirms_.clear();
  loop_.add_fd(fd, EPOLLIN | EPOLLRDHUP, [this](uint32_t ev) { on_client_io(ev); });
}

void DeconzServer::close_client() {
  if (client_fd_ < 0) return;
  loop_.del_fd(client_fd_);
  ::close(client_fd_);
  client_fd_ = -1;
  in_.clear();
  out_.clear();
  indications_.clear();
  confirms_.clear();
}

void DeconzServer::on_client_io(uint32_t events) {
  if (events & EPOLLOUT) flush();
  if (client_fd_ < 0) return;
  if (events & EPOLLIN) {
    uint8_t buf[1024];
    for (;;) {
      ssize_t n = ::read(client_fd_, buf, sizeof buf);
      if (n == 0 || (n < 0 && errno != EAGAIN && errno != EINTR)) {
        LOGI("zigbee: client disconnected");
        return close_client();
      }
      if (n < 0) {
        if (errno == EINTR) continue;
        break;
      }
      for (ssize_t i = 0; i < n; i++) {
        if (buf[i] == kEnd) {
          if (!in_.empty()) on_slip(in_);
          in_.clear();
          if (client_fd_ < 0) return;
        } else if (in_.size() < 1024) {
          in_.push_back(buf[i]);
        }
      }
    }
  }
  if (events & (EPOLLHUP | EPOLLERR | EPOLLRDHUP)) {
    LOGI("zigbee: client disconnected");
    close_client();
  }
}

void DeconzServer::flush() {
  while (!out_.empty() && client_fd_ >= 0) {
    ssize_t w = ::send(client_fd_, out_.data(), out_.size(), MSG_NOSIGNAL);
    if (w < 0 && errno == EINTR) continue;
    if (w < 0 && errno == EAGAIN) break;
    if (w <= 0) return close_client();
    out_.erase(0, size_t(w));
  }
  if (client_fd_ >= 0) loop_.mod_fd(client_fd_, EPOLLIN | EPOLLRDHUP | (out_.empty() ? 0 : EPOLLOUT));
}

void DeconzServer::on_slip(const std::vector<uint8_t> &raw) {
  std::vector<uint8_t> f;
  f.reserve(raw.size());
  for (size_t i = 0; i < raw.size(); i++) {
    uint8_t b = raw[i];
    if (b == kEsc && i + 1 < raw.size()) {
      b = raw[++i] == kEscEnd ? kEnd : raw[i] == kEscEsc ? kEsc : raw[i];
    }
    f.push_back(b);
  }
  // Too short or a bad checksum: noise, or the EZSP/ZNP probes ZHA sends to unknown radios.
  if (f.size() < 7) return;
  uint16_t sum = 0;
  for (size_t i = 0; i < f.size() - 2; i++) sum = uint16_t(sum + f[i]);
  if (uint16_t(~sum + 1) != get_le16(f.data() + f.size() - 2)) return;
  f.resize(f.size() - 2);
  handle(f[0], f[1], f.data() + 5, f.size() - 5);
}

void DeconzServer::reply(uint8_t cmd, uint8_t seq, uint8_t status, const std::vector<uint8_t> &body, bool payload_length) {
  std::vector<uint8_t> f{cmd, seq, status};
  put_le16(f, uint16_t(5 + (payload_length ? 2 : 0) + body.size()));
  if (payload_length) put_le16(f, uint16_t(body.size()));
  f.insert(f.end(), body.begin(), body.end());
  uint16_t sum = 0;
  for (uint8_t b : f) sum = uint16_t(sum + b);
  put_le16(f, uint16_t(~sum + 1));
  out_.push_back(char(kEnd));
  for (uint8_t b : f) {
    if (b == kEnd) {
      out_.push_back(char(kEsc));
      out_.push_back(char(kEscEnd));
    } else if (b == kEsc) {
      out_.push_back(char(kEsc));
      out_.push_back(char(kEscEsc));
    } else {
      out_.push_back(char(b));
    }
  }
  out_.push_back(char(kEnd));
  flush();
}

uint8_t DeconzServer::network_state() const {
  if (!online_) return kOffline;
  if (formation_ != Formation::NONE) return kJoining;
  return coord_.ready() && coord_.network().formed ? kConnected : kOffline;
}

uint8_t DeconzServer::device_state() const {
  uint8_t s = network_state();
  if (!confirms_.empty()) s |= kFlagConfirm;
  if (!indications_.empty()) s |= kFlagIndication;
  if (coord_.pending() < kMaxInFlight) s |= kFlagFreeSlots;
  return s;
}

void DeconzServer::notify_state() {
  if (client_fd_ < 0) return;
  const uint8_t s = device_state();
  if (s == last_state_) return;
  last_state_ = s;
  // seq 0: zigpy numbers its requests 1..255, so this never matches one of them.
  reply(kDeviceStateChanged, 0, kSuccess, {s, 0}, false);
}

void DeconzServer::handle(uint8_t cmd, uint8_t seq, const uint8_t *p, size_t n) {
  // p: after [command][seq][status][frame length]; commands with a payload length start with it.
  switch (cmd) {
    case kVersion:
      reply(cmd, seq, kSuccess, [] {
        std::vector<uint8_t> v;
        put_le32(v, kFirmwareVersion);
        return v;
      }(), false);
      return;
    case kDeviceState:
      last_state_ = device_state();
      reply(cmd, seq, kSuccess, {last_state_, 0, 0}, false);
      return;
    case kChangeNetworkState:
      if (n >= 1) change_network_state(seq, p[0]);
      return;
    case kReadParameter:
      if (n >= 3) read_parameter(seq, p + 2, n - 2);
      return;
    case kWriteParameter:
      if (n >= 3) write_parameter(seq, p + 2, n - 2);
      return;
    case kApsDataRequest:
      if (n >= 2) aps_data_request(seq, p + 2, n - 2, get_le16(p));
      return;
    case kApsDataIndication: {
      if (indications_.empty()) {
        reply(cmd, seq, kFailure, {}, true);
        return;
      }
      const ApsFrame f = std::move(indications_.front().frame);
      indications_.pop_front();
      last_state_ = device_state();
      std::vector<uint8_t> b{last_state_};
      b.push_back(f.group ? 0x01 : 0x02);  // group / NWK
      put_le16(b, f.dst);
      b.push_back(f.dst_ep);
      b.push_back(0x02);
      put_le16(b, f.src);
      b.push_back(f.src_ep);
      put_le16(b, f.profile);
      put_le16(b, f.cluster);
      put_le16(b, uint16_t(f.data.size()));
      b.insert(b.end(), f.data.begin(), f.data.end());
      b.insert(b.end(), {0, 0, f.lqi, 0, 0, 0, 0, uint8_t(f.rssi)});
      reply(cmd, seq, kSuccess, b, true);
      return;
    }
    case kApsDataConfirm: {
      if (confirms_.empty()) {
        reply(cmd, seq, kFailure, {}, true);
        return;
      }
      const Confirm c = confirms_.front();
      confirms_.pop_front();
      last_state_ = device_state();
      std::vector<uint8_t> b{last_state_, c.req.req_id, c.req.dst_mode};
      if (c.req.dst_mode == 0x03) {
        put_le64(b, c.req.dst_addr);
      } else {
        put_le16(b, uint16_t(c.req.dst_addr));
      }
      if (c.req.dst_mode != 0x01) b.push_back(c.req.dst_ep);
      b.insert(b.end(), {c.req.src_ep, c.status, 0, 0, 0, 0});
      reply(cmd, seq, kSuccess, b, true);
      return;
    }
    default:
      LOGD("zigbee: deCONZ command 0x%02x not supported", cmd);
      return;
  }
}

void DeconzServer::read_parameter(uint8_t seq, const uint8_t *p, size_t n) {
  const uint8_t id = p[0];
  const ZigbeeNetwork &net = coord_.network();
  const bool formed = coord_.ready() && net.formed;
  std::vector<uint8_t> v{id};
  uint8_t status = kSuccess;
  switch (id) {
    case kMacAddress:
      put_le64(v, net.ieee);
      break;
    case kNwkPanId:
      put_le16(v, formed ? net.pan : 0xFFFF);
      break;
    case kNwkAddress:
      put_le16(v, formed ? net.nwk : 0xFFFF);
      break;
    case kNwkExtPanId:
    case kApsExtPanId:
      put_le64(v, formed ? net.ext_pan : ~0ull);
      break;
    case kDesignedCoordinator:
      v.push_back(1);
      break;
    case kChannelMask:
      put_le32(v, channel_mask_ ? channel_mask_ : formed ? 1u << net.channel : 0);
      break;
    case kTrustCenterAddress:
      put_le64(v, net.ieee);
      break;
    case kSecurityMode:
      v.push_back(kSecurityOnlyTclk);
      break;
    case kConfigureEndpoint: {
      const size_t slot = n >= 2 ? p[1] : 0xFF;
      if (slot >= kEndpointSlots) {
        status = kUnsupported;
        break;
      }
      v.push_back(uint8_t(slot));
      put_descriptor(v, slots_[slot].value_or(SimpleDescriptor{}));
      break;
    }
    case kPredefinedPanId:
      v.push_back(1);
      break;
    case kNetworkKey:
      v.push_back(n >= 2 ? p[1] : 0);
      if (formed) {
        v.insert(v.end(), net.key.begin(), net.key.end());
      } else {
        v.insert(v.end(), 16, 0xFF);
      }
      break;
    case kLinkKey:
      if (n < 9) {
        status = kInvalidValue;
        break;
      }
      v.insert(v.end(), p + 1, p + 9);
      v.insert(v.end(), kTcLinkKey, kTcLinkKey + 16);
      break;
    case kCurrentChannel:
      v.push_back(formed ? net.channel : 0);
      break;
    case kPermitJoin:
      v.push_back(0);
      break;
    case kProtocolVersion:
      put_le16(v, kProtocol);
      break;
    case kNwkUpdateId:
      v.push_back(formed ? net.update_id : update_id_);
      break;
    case kFrameCounter:
      put_le32(v, net.frame_counter);
      break;
    case kZdpResponseHandling:
      put_le16(v, 0);
      break;
    default:
      status = kUnsupported;
      break;
  }
  if (status != kSuccess) v.resize(1);
  reply(kReadParameter, seq, status, v, true);
}

void DeconzServer::write_parameter(uint8_t seq, const uint8_t *p, size_t n) {
  const uint8_t id = p[0];
  const uint8_t *v = p + 1;
  const size_t vn = n - 1;
  const ZigbeeNetwork &net = coord_.network();
  const bool formed = coord_.ready() && net.formed;
  uint8_t status = kSuccess;
  auto need = [&](size_t k) {
    if (vn >= k) return true;
    status = kInvalidValue;
    return false;
  };
  switch (id) {
    case kMacAddress:
      if (!need(8)) break;
      ieee_ = get_le64(v);
      if (*ieee_ != net.ieee) new_network_ = true;
      break;
    case kNwkAddress:
      if (need(2) && get_le16(v) != 0x0000) status = kUnsupported;
      break;
    case kDesignedCoordinator:
      if (need(1) && v[0] != 1) status = kUnsupported;
      break;
    case kFrameCounter:
      if (need(4)) frame_counter_ = get_le32(v);
      break;
    case kNwkPanId:
      if (!need(2)) break;
      pan_ = get_le16(v);
      if (!formed || *pan_ != net.pan) new_network_ = true;
      break;
    case kNwkExtPanId:
    case kApsExtPanId:
      if (!need(8)) break;
      ext_pan_ = get_le64(v);
      if (!formed || *ext_pan_ != net.ext_pan) new_network_ = true;
      break;
    case kNetworkKey:  // [index][key]
      if (!need(17)) break;
      key_.emplace();
      std::copy(v + 1, v + 17, key_->begin());
      if (!formed || *key_ != net.key) new_network_ = true;
      break;
    case kChannelMask:
      if (need(4)) channel_mask_ = get_le32(v);
      break;
    case kNwkUpdateId:
      if (need(1)) update_id_ = v[0];
      break;
    case kLinkKey:
      // Only the global trust center link key; no install-code keys.
      if (need(24) && memcmp(v + 8, kTcLinkKey, 16) != 0) status = kUnsupported;
      break;
    case kConfigureEndpoint: {
      SimpleDescriptor d;
      if (!need(2) || v[0] >= kEndpointSlots || !get_descriptor(v + 1, vn - 1, d)) {
        if (status == kSuccess) status = kInvalidValue;
        break;
      }
      set_slot(v[0], d);
      break;
    }
    case kPermitJoin:
      if (need(1)) coord_.permit_join(v[0]);
      break;
    case kTrustCenterAddress:
    case kSecurityMode:
    case kPredefinedPanId:
    case kWatchdogTtl:
    case kZdpResponseHandling:
      break;
    default:
      status = kUnsupported;
      break;
  }
  reply(kWriteParameter, seq, status, {id}, true);
}

NetworkConfig DeconzServer::next_network() const {
  const ZigbeeNetwork &net = coord_.network();
  NetworkConfig c;
  c.channel = form_channel();
  c.pan = pan_.value_or(net.formed ? net.pan : 0xFFFF);
  c.ext_pan = ext_pan_.value_or(net.formed ? net.ext_pan : 0);
  c.key = key_.value_or(net.formed ? net.key : NetworkKey{});
  c.ieee = ieee_.value_or(net.ieee);  // hosts read it before forming and keep it in backups
  // The erase restores the counter last saved to NV, behind the running one: devices that keep the
  // key would drop frames until it catches up. 1024: the stack's own NV margin.
  c.frame_counter = std::max(frame_counter_, net.formed ? net.frame_counter + 1024 : 0);
  c.update_id = update_id_;
  return c;
}

uint8_t DeconzServer::form_channel() const {
  for (uint8_t ch = 11; ch <= 26; ch++)
    if (channel_mask_ & (1u << ch)) return ch;
  return 15;
}

void DeconzServer::change_network_state(uint8_t seq, uint8_t target) {
  const ZigbeeNetwork &net = coord_.network();
  if (target == kOffline) {
    online_ = false;
  } else if (target == kConnected) {
    online_ = true;
    // zigpy forms a network by writing its parameters, then OFFLINE -> CONNECTED; a channel mask
    // of 0 (with PAN ID 0xFFFF) only erases the old one. The network to form is fixed now: the
    // erase takes the current values with it.
    if (new_network_ || !net.formed) {
      pending_ = next_network();
      if (net.formed) {
        coord_.factory_reset();
        formation_ = channel_mask_ ? Formation::AWAIT_ERASED : Formation::NONE;
      } else if (channel_mask_) {
        formation_ = Formation::AWAIT_ERASED;  // already erased: forms on the next step
      }
      new_network_ = false;
    } else if (channel_mask_ && __builtin_popcount(channel_mask_) == 1 && !(channel_mask_ & (1u << net.channel))) {
      coord_.change_channel(form_channel(), update_id_);
    }
  }
  reply(kChangeNetworkState, seq, kSuccess, {target}, false);
  on_changed();
}

void DeconzServer::aps_data_request(uint8_t seq, const uint8_t *p, size_t n, size_t declared) {
  // [req id][flags][dst mode][dst][dst ep unless group][profile][cluster][src ep][asdu len][asdu]
  // [tx options][radius][relays if flags & 2]
  auto fail = [&](uint8_t status, uint8_t req_id) { reply(kApsDataRequest, seq, status, {device_state(), req_id}, true); };
  if (n < 3) return fail(kInvalidValue, 0);
  Request r{};
  r.req_id = p[0];
  r.dst_mode = p[2];
  size_t i = 3;
  // zigbee-herdsman sends NWK_addr_req with NWK mode but the IEEE address in place of the NWK
  // one (the payload is 6 bytes longer than it declares): sent as the broadcast it is meant to be.
  const bool herdsman_nwk_addr = r.dst_mode == 0x02 && n == declared + 6;
  if (herdsman_nwk_addr) {
    if (n < i + 8) return fail(kInvalidValue, r.req_id);
    r.dst_addr = 0xFFFD;
    i += 8;
  } else if (r.dst_mode == 0x01 || r.dst_mode == 0x02) {
    if (n < i + 2) return fail(kInvalidValue, r.req_id);
    r.dst_addr = get_le16(p + i);
    i += 2;
  } else if (r.dst_mode == 0x03) {
    if (n < i + 8) return fail(kInvalidValue, r.req_id);
    r.dst_addr = get_le64(p + i);
    i += 8;
  } else {
    return fail(kInvalidValue, r.req_id);
  }
  if (r.dst_mode != 0x01) {
    if (n < i + 1) return fail(kInvalidValue, r.req_id);
    r.dst_ep = p[i++];
  }
  if (n < i + 7) return fail(kInvalidValue, r.req_id);
  ApsFrame f;
  f.profile = get_le16(p + i);
  f.cluster = get_le16(p + i + 2);
  f.src_ep = p[i + 4];
  r.src_ep = f.src_ep;
  const size_t len = get_le16(p + i + 5);
  i += 7;
  if (n < i + len) return fail(kInvalidValue, r.req_id);
  f.data.assign(p + i, p + i + len);
  i += len;
  if (n >= i + 2) {
    f.tx_options = p[i];
    f.radius = p[i + 1];
  }
  f.dst_ep = r.dst_ep;
  f.group = r.dst_mode == 0x01;
  f.dst = r.dst_mode == 0x03 ? 0 : uint16_t(r.dst_addr);
  const uint32_t handle = generation_ << 8 | r.req_id;
  requests_[handle] = r;
  if (!coord_.send(handle, std::move(f), r.dst_mode == 0x03 ? r.dst_addr : 0)) {
    requests_.erase(handle);
    return fail(kFailure, r.req_id);
  }
  reply(kApsDataRequest, seq, kSuccess, {device_state(), r.req_id}, true);
}

void DeconzServer::on_received(const ApsFrame &f) {
  if (client_fd_ < 0 || !online_) return;
  if (indications_.size() >= kMaxQueued) indications_.pop_front();
  indications_.push_back({f});
  notify_state();
}

void DeconzServer::on_sent(uint32_t handle, uint8_t status) {
  auto it = requests_.find(handle);
  if (it == requests_.end()) return;
  const Request r = it->second;
  requests_.erase(it);
  if ((handle >> 8) != generation_ || client_fd_ < 0) return;  // a previous client's request
  if (confirms_.size() >= kMaxQueued) confirms_.pop_front();
  confirms_.push_back({r, status});
  notify_state();
}

void DeconzServer::on_changed() {
  if (coord_.ready() && formation_ == Formation::AWAIT_ERASED && !coord_.network().formed) {
    formation_ = Formation::FORMING;
    coord_.form(pending_, [this](bool ok) {
      if (!ok) LOGW("zigbee: the radio failed to form the network");
      formation_ = Formation::NONE;
      notify_state();
    });
  }
  // The radio restarted outside a formation: drop the host, it reconnects once the state is known.
  if (!coord_.ready() && formation_ == Formation::NONE && client_fd_ >= 0) {
    LOGI("zigbee: radio restarting, dropping the client");
    close_client();
  }
  notify_state();
}

}  // namespace cs
