#include "zigbee/ezsp_router.h"

#include <fcntl.h>
#include <poll.h>
#include <termios.h>
#include <unistd.h>

#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <vector>

#include "core/log.h"

namespace cs {

namespace {

using Bytes = std::vector<uint8_t>;
using Clock = std::chrono::steady_clock;

constexpr uint16_t kVersion = 0x0000, kSetConfig = 0x0053, kNetworkInit = 0x0017,
                   kNetworkState = 0x0018, kStackStatus = 0x0019, kStartScan = 0x001A, kNetworkFound = 0x001B,
                   kScanComplete = 0x001C, kJoinNetwork = 0x001F, kLeaveNetwork = 0x0020, kGetNodeId = 0x0027,
                   kGetNetworkParams = 0x0028, kSetSecurity = 0x0068;
constexpr uint8_t kStackUp = 0x90, kStackDown = 0x91;
constexpr uint8_t kNodeRouter = 2;
constexpr uint32_t kChannels = 0x07FFF800;  // 11..26
constexpr int kTxPowerDbm = 8;

bool reserved(uint8_t b) { return b == 0x7E || b == 0x11 || b == 0x13 || b == 0x18 || b == 0x1A || b == 0x7D; }

uint16_t crc16(const uint8_t *p, size_t n) {
  uint16_t crc = 0xFFFF;
  for (size_t i = 0; i < n; i++) {
    crc ^= uint16_t(p[i]) << 8;
    for (int k = 0; k < 8; k++) crc = (crc & 0x8000) ? uint16_t((crc << 1) ^ 0x1021) : uint16_t(crc << 1);
  }
  return crc;
}

// ASH data randomisation: XOR with an LFSR sequence restarted for every frame.
void randomize(Bytes &d) {
  uint8_t r = 0x42;
  for (auto &b : d) {
    b ^= r;
    r = (r & 1) ? uint8_t((r >> 1) ^ 0xB8) : uint8_t(r >> 1);
  }
}

int64_t ms_since(Clock::time_point t) {
  return std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now() - t).count();
}

}  // namespace

// Everything below runs on the worker thread.
struct EzspRouter::Impl {
  EzspRouter &o;
  int fd = -1;
  Bytes rx;
  unsigned frm = 0, ack = 0;  // next DATA frame number to send / expected from the NCP
  uint8_t seq = 0;
  bool joined = false, node_dirty = false, reset_seen = false, reset_ok = false;
  uint16_t node_id = 0xFFFF;
  std::vector<std::pair<uint16_t, Bytes>> pending_cb;  // callbacks seen while waiting for a response

  explicit Impl(EzspRouter &r) : o(r) {}
  ~Impl() { close_tty(); }

  void close_tty() {
    if (fd >= 0) ::close(fd);
    fd = -1;
  }

  bool open_tty() {
    fd = ::open(o.tty_.c_str(), O_RDWR | O_NOCTTY | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0) return false;
    termios t{};
    tcgetattr(fd, &t);
    cfmakeraw(&t);
    const speed_t sp = o.baud_ == 460800 ? B460800 : o.baud_ == 230400 ? B230400 : B115200;
    cfsetispeed(&t, sp);
    cfsetospeed(&t, sp);
    t.c_cflag |= CLOCAL | CREAD;
    t.c_cflag &= ~CRTSCTS;
    t.c_cc[VMIN] = 0;
    t.c_cc[VTIME] = 0;
    tcsetattr(fd, TCSANOW, &t);
    tcflush(fd, TCIOFLUSH);
    return true;
  }

  // ---------------------------------------------------------------- ASH
  void write_raw(const Bytes &b) {
    size_t off = 0;
    while (off < b.size() && !o.stop_) {
      const ssize_t n = ::write(fd, b.data() + off, b.size() - off);
      if (n > 0) {
        off += size_t(n);
      } else if (n < 0 && (errno == EAGAIN || errno == EINTR)) {
        pollfd p{fd, POLLOUT, 0};
        poll(&p, 1, 100);
      } else {
        return;
      }
    }
  }

  void send_frame(uint8_t ctrl, const Bytes &data) {
    Bytes body;
    body.push_back(ctrl);
    body.insert(body.end(), data.begin(), data.end());
    const uint16_t crc = crc16(body.data(), body.size());
    body.push_back(uint8_t(crc >> 8));
    body.push_back(uint8_t(crc));
    Bytes out;
    for (uint8_t b : body) {
      if (reserved(b)) {
        out.push_back(0x7D);
        out.push_back(b ^ 0x20);
      } else {
        out.push_back(b);
      }
    }
    out.push_back(0x7E);
    write_raw(out);
  }

  // One decoded frame (control + data, CRC checked) from the byte stream, waiting up to timeout_ms.
  bool read_frame(Bytes &frame, int timeout_ms) {
    const auto t0 = Clock::now();
    for (;;) {
      auto it = std::find(rx.begin(), rx.end(), uint8_t(0x7E));
      if (it != rx.end()) {
        Bytes raw(rx.begin(), it);
        rx.erase(rx.begin(), it + 1);
        const auto cancel = std::find(raw.rbegin(), raw.rend(), uint8_t(0x1A));
        if (cancel != raw.rend()) raw.erase(raw.begin(), raw.end() - (cancel - raw.rbegin()));
        Bytes un;
        bool esc = false;
        for (uint8_t b : raw) {
          if (b == 0x7D) {
            esc = true;
            continue;
          }
          if (esc) {
            b ^= 0x20;
            esc = false;
          }
          if (b == 0x11 || b == 0x13) continue;
          un.push_back(b);
        }
        if (un.size() < 3 || crc16(un.data(), un.size()) != 0) continue;
        un.resize(un.size() - 2);
        frame = std::move(un);
        return true;
      }
      const int left = timeout_ms - int(ms_since(t0));
      if (left <= 0 || o.stop_) return false;
      pollfd p{fd, POLLIN, 0};
      if (poll(&p, 1, std::min(left, 100)) > 0) {
        uint8_t buf[256];
        const ssize_t n = ::read(fd, buf, sizeof buf);
        if (n > 0) rx.insert(rx.end(), buf, buf + n);
        else if (n == 0 || (errno != EAGAIN && errno != EINTR)) return false;
      }
    }
  }

  bool ash_reset() {
    reset_ok = true;  // the RSTACK answering our own RST is expected
    struct Guard { bool &f; ~Guard() { f = false; } } guard{reset_ok};
    rx.clear();
    frm = ack = 0;
    for (int i = 0; i < 3; i++) {
      write_raw({0x1A, 0xC0, 0x38, 0xBC, 0x7E});
      Bytes f;
      const auto t0 = Clock::now();
      while (ms_since(t0) < 2000 && read_frame(f, 2000 - int(ms_since(t0))))
        if (!f.empty() && f[0] == 0xC1) return true;
    }
    return false;
  }

  // A DATA frame from the NCP: acknowledge it and return its payload; other frames are consumed.
  bool handle_frame(const Bytes &f, Bytes *payload, bool *acked, bool *reset) {
    const uint8_t c = f[0];
    if (!(c & 0x80)) {  // DATA
      const unsigned n = (c >> 4) & 7;
      if (n == ack) {
        ack = (ack + 1) & 7;
        if (payload) {
          payload->assign(f.begin() + 1, f.end());
          randomize(*payload);
        }
      }
      send_frame(uint8_t(0x80 | ack), {});
      // the NCP's ackNum is the next frame it expects: our `frm` + 1 acknowledges the frame in flight
      if (acked && (c & 7) == ((frm + 1) & 7)) *acked = true;
      return n == ack || ((ack - 1) & 7) == n;
    }
    if ((c & 0xE0) == 0x80) {  // ACK
      if (acked && (c & 7) == ((frm + 1) & 7)) *acked = true;
    } else if (c == 0xC1 || c == 0xC2) {
      if (!reset_ok) {
        char b[64];
        snprintf(b, sizeof b, "%02x", c);
        std::string hex = b;
        for (size_t i = 1; i < f.size() && i < 6; i++) { snprintf(b, sizeof b, " %02x", f[i]); hex += b; }
        LOGW("zigbee: NCP sent %s frame (%s)", c == 0xC1 ? "RSTACK" : "ERROR", hex.c_str());
      }
      reset_seen = true;
      if (reset) *reset = true;
    }
    return false;
  }

  bool send_data(const Bytes &payload) {
    Bytes data = payload;
    randomize(data);
    for (int attempt = 0; attempt < 4 && !o.stop_; attempt++) {
      const unsigned mine = frm;
      send_frame(uint8_t((mine << 4) | (attempt ? 8 : 0) | ack), data);
      const unsigned next = (mine + 1) & 7;
      const auto t0 = Clock::now();
      bool acked = false;
      while (!acked && ms_since(t0) < 700) {
        Bytes f;
        if (!read_frame(f, 700 - int(ms_since(t0)))) break;
        Bytes pl;
        bool reset = false;
        // `frm` is still `mine` while waiting: handle_frame compares against it
        const bool dat = handle_frame(f, &pl, &acked, &reset);
        if (reset) return false;
        if (dat && !pl.empty()) queue_ezsp(pl);
      }
      if (acked) {
        frm = next;
        return true;
      }
    }
    return false;
  }

  // ---------------------------------------------------------------- EZSP (v8, extended format)
  std::vector<Bytes> rxq;  // EZSP frames received while waiting for something else
  void queue_ezsp(const Bytes &p) { rxq.push_back(p); }

  static bool parse(const Bytes &p, uint8_t &s, uint8_t &fc, uint16_t &id, Bytes &body) {
    if (p.size() < 5) return false;
    s = p[0];
    fc = p[1];
    id = uint16_t(p[3] | (p[4] << 8));
    body.assign(p.begin() + 5, p.end());
    return true;
  }

  void dispatch_callback(uint16_t id, const Bytes &b) {
    if (id == kStackStatus && !b.empty()) {
      if (b[0] == kStackUp) {
        joined = true;
        node_dirty = true;  // read by the top level: a nested call here could take another response
      } else if (b[0] == kStackDown) {
        joined = false;
      }
    }
    pending_cb.emplace_back(id, b);
  }

  void drain(int timeout_ms) {  // callbacks while idle
    for (;;) {
      Bytes f;
      if (rxq.empty()) {
        if (!read_frame(f, timeout_ms)) return;
        Bytes pl;
        if (handle_frame(f, &pl, nullptr, nullptr) && !pl.empty()) rxq.push_back(pl);
        timeout_ms = 0;
        if (rxq.empty()) return;
      }
      Bytes p = std::move(rxq.front());
      rxq.erase(rxq.begin());
      uint8_t s, fc;
      uint16_t id;
      Bytes body;
      if (parse(p, s, fc, id, body)) dispatch_callback(id, body);
    }
  }

  bool call(uint16_t id, const Bytes &params, Bytes &resp, int timeout_ms = 3000) {
    Bytes frame = {seq, 0x00, 0x01, uint8_t(id), uint8_t(id >> 8)};
    frame.insert(frame.end(), params.begin(), params.end());
    const uint8_t mine = seq++;
    if (!send_data(frame)) return false;
    const auto t0 = Clock::now();
    for (;;) {
      Bytes p;
      if (!rxq.empty()) {
        p = std::move(rxq.front());
        rxq.erase(rxq.begin());
      } else {
        const int left = timeout_ms - int(ms_since(t0));
        Bytes f;
        if (left <= 0 || !read_frame(f, left)) return false;
        Bytes pl;
        bool reset = false;
        const bool dat = handle_frame(f, &pl, nullptr, &reset);
        if (reset) return false;
        if (!dat || pl.empty()) continue;
        p = std::move(pl);
      }
      uint8_t s, fc;
      uint16_t rid;
      Bytes body;
      if (!parse(p, s, fc, rid, body)) continue;
      if ((fc & 0x80) && !(fc & 0x10) && s == mine && rid == id) {
        resp = std::move(body);
        return true;
      }
      dispatch_callback(rid, body);
    }
  }

  bool version(uint8_t desired) {
    Bytes frame = {seq, 0x00, 0x00, desired};  // legacy format for the very first command
    const uint8_t mine = seq++;
    if (!send_data(frame)) return false;
    const auto t0 = Clock::now();
    while (ms_since(t0) < 3000) {
      Bytes pl;
      if (!rxq.empty()) {  // the response may already have arrived while waiting for the ACK
        pl = std::move(rxq.front());
        rxq.erase(rxq.begin());
      } else {
        Bytes f;
        if (!read_frame(f, 3000 - int(ms_since(t0)))) return false;
        handle_frame(f, &pl, nullptr, nullptr);
      }
      if (pl.size() >= 7 && pl[0] == mine && pl[2] == 0x00) {
        if (pl[3] < 8) return false;  // only the extended frame format is implemented
        Bytes r;
        return call(kVersion, {pl[3]}, r);  // confirm the version in extended format
      }
    }
    return false;
  }

  bool set_config(uint8_t id, uint16_t v) {
    Bytes r;
    return call(kSetConfig, {id, uint8_t(v), uint8_t(v >> 8)}, r) && !r.empty() && r[0] == 0;
  }

  void read_node_id() {
    Bytes r;
    if (call(kGetNodeId, {}, r) && r.size() >= 2) node_id = uint16_t(r[0] | (r[1] << 8));
  }

  bool net_state(uint8_t &st) {
    Bytes r;
    if (!call(kNetworkState, {}, r) || r.empty()) return false;
    st = r[0];
    return true;
  }

  std::string describe_joined() {
    Bytes r;
    char b[96];
    if (call(kGetNetworkParams, {}, r) && r.size() >= 14 && r[0] == 0) {
      const unsigned pan = r[10] | (r[11] << 8);
      snprintf(b, sizeof b, "Router: joined, ch %u, PAN 0x%04x, node 0x%04x", r[13], pan, node_id);
    } else {
      snprintf(b, sizeof b, "Router: joined, node 0x%04x", node_id);
    }
    return b;
  }

  // Resets the NCP link, sets the stack up and resumes the network kept in the NCP's flash.
  bool init() {
    if (!ash_reset()) return false;
    rxq.clear();
    pending_cb.clear();
    if (!version(8)) return false;
    set_config(0x0C, 2);  // stack profile 2 (ZigBee PRO): the NCP's default is 0
    set_config(0x0D, 5);  // security level 5
    set_config(0x2D, 1);  // supported networks
    Bytes r;
    if (!call(kNetworkInit, {0, 0}, r) || r.empty()) return false;
    joined = false;
    if (r[0] == 0) {  // resuming: NETWORK_UP follows as a callback
      const auto t0 = Clock::now();
      while (!joined && ms_since(t0) < 8000 && !o.stop_) drain(200);
    }
    uint8_t st = 0;
    if (net_state(st)) joined = st == 2;
    if (joined) read_node_id();
    node_dirty = false;
    reset_seen = false;
    return true;
  }

  bool do_join() {
    uint8_t st = 0;
    if (net_state(st) && st == 2) {
      o.set_status(describe_joined());
      return true;
    }
    o.set_status("Router: scanning for a network with permit join open...");
    Bytes r;
    pending_cb.clear();
    // security: the well-known ZigBee 3.0 trust centre link key
    Bytes sec = {0x00, 0x01};
    const char *key = "ZigBeeAlliance09";
    sec.insert(sec.end(), key, key + 16);
    sec.insert(sec.end(), 16 + 1 + 8, 0);
    if (!call(kSetSecurity, sec, r) || r.empty() || r[0] != 0) {
      o.set_status("Router: cannot set security state");
      return false;
    }
    if (!call(kStartScan, {1, uint8_t(kChannels), uint8_t(kChannels >> 8), uint8_t(kChannels >> 16),
                           uint8_t(kChannels >> 24), 4}, r) || r.empty() || r[0] != 0) {
      o.set_status("Router: scan failed");
      return false;
    }
    struct Found {
      Bytes net;  // channel, pan(2), xpan(8), allow, profile, upd
      int lqi;
    };
    std::vector<Found> found;
    bool done = false;
    const auto t0 = Clock::now();
    while (!done && ms_since(t0) < 30000 && !o.stop_) {
      drain(300);
      for (auto &cb : pending_cb) {
        if (cb.first == kNetworkFound && cb.second.size() >= 16)
          found.push_back({Bytes(cb.second.begin(), cb.second.begin() + 14), cb.second[14]});
        else if (cb.first == kScanComplete)
          done = true;
      }
      pending_cb.clear();
    }
    const Found *best = nullptr;
    for (const auto &f : found)
      if (f.net[11] == 1 && f.net[12] == 2 && (!best || f.lqi > best->lqi)) best = &f;
    if (!best) {
      o.set_status("Router: no network with permit join open (open it in Zigbee2MQTT / ZHA, then press Zigbee join network)");
      return false;
    }
    Bytes p = {kNodeRouter};
    p.insert(p.end(), best->net.begin() + 3, best->net.begin() + 11);       // extended PAN ID
    p.push_back(best->net[1]);                                               // PAN ID
    p.push_back(best->net[2]);
    p.push_back(uint8_t(kTxPowerDbm));
    p.push_back(best->net[0]);                                               // channel
    p.push_back(0);                                                          // MAC association
    p.push_back(0);                                                          // NWK manager ID
    p.push_back(0);
    p.push_back(best->net[13]);                                              // NWK update ID
    for (int i = 0; i < 4; i++) p.push_back(uint8_t(kChannels >> (8 * i)));
    char b[96];
    snprintf(b, sizeof b, "Router: joining ch %u PAN 0x%04x...", best->net[0], best->net[1] | (best->net[2] << 8));
    o.set_status(b);
    joined = false;
    if (!call(kJoinNetwork, p, r) || r.empty() || r[0] != 0) {
      o.set_status("Router: join command refused");
      return false;
    }
    bool failed = false;
    uint8_t why = 0;
    const auto t1 = Clock::now();
    while (!joined && !failed && ms_since(t1) < 45000 && !o.stop_) {
      drain(300);
      for (auto &cb : pending_cb)
        if (cb.first == kStackStatus && !cb.second.empty() && cb.second[0] != kStackUp) {
          failed = true;
          why = cb.second[0];
        }
      pending_cb.clear();
    }
    if (!joined) {
      snprintf(b, sizeof b, "Router: join failed (0x%02x), open permit join and retry", why);
      o.set_status(b);
      return false;
    }
    read_node_id();
    node_dirty = false;
    o.set_status(describe_joined());
    return true;
  }

  void do_leave() {
    Bytes r;
    pending_cb.clear();
    if (call(kLeaveNetwork, {}, r)) {
      const auto t0 = Clock::now();
      while (joined && ms_since(t0) < 5000) drain(200);
    }
    joined = false;
    o.set_status("Router: left the network (press Zigbee join network to join again)");
  }

  std::string idle_status() {
    return joined ? describe_joined()
                  : "Router: not in a network (open permit join in Zigbee2MQTT / ZHA, then press Zigbee join network)";
  }
};

EzspRouter::EzspRouter(Loop &loop, std::string tty, unsigned baud, std::function<void()> changed)
    : loop_(loop), tty_(std::move(tty)), baud_(baud), changed_(std::move(changed)) {}

EzspRouter::~EzspRouter() {
  *alive_ = false;
  stop_ = true;
  if (thread_.joinable()) thread_.join();
}

void EzspRouter::start() { thread_ = std::thread([this] { run(); }); }

void EzspRouter::request(Cmd c) { cmd_ = int(c); }

std::string EzspRouter::status() const {
  std::lock_guard<std::mutex> lk(mu_);
  return status_;
}

void EzspRouter::set_status(const std::string &s) {
  {
    std::lock_guard<std::mutex> lk(mu_);
    if (s == status_) return;
    status_ = s;
  }
  auto alive = alive_;
  loop_.post([this, alive] {
    if (*alive && changed_) changed_();
  });
}

void EzspRouter::run() {
  Impl d(*this);
  while (!stop_) {
    if (!d.open_tty()) {
      set_status("Router: cannot open " + tty_);
      for (int i = 0; i < 50 && !stop_; i++) usleep(100000);
      continue;
    }
    set_status("Router: starting radio...");
    if (!d.init()) {
      set_status("Router: radio not answering, retrying");
      d.close_tty();
      for (int i = 0; i < 50 && !stop_; i++) usleep(100000);
      continue;
    }
    set_status(d.idle_status());
    auto last_check = Clock::now();
    bool ok = true;
    while (ok && !stop_) {
      d.drain(250);
      if (d.reset_seen) {
        LOGW("zigbee: the NCP reset itself or reported an ASH error");
        ok = false;
        break;
      }
      if (d.node_dirty) {
        d.node_dirty = false;
        d.read_node_id();
      }
      for (auto &cb : d.pending_cb)
        if (cb.first == kStackStatus && !cb.second.empty()) set_status(d.idle_status());
      d.pending_cb.clear();
      const int c = cmd_.exchange(0);
      if (c == int(Cmd::JOIN)) {
        d.do_join();
        if (!d.joined) continue;
      } else if (c == int(Cmd::LEAVE)) {
        d.do_leave();
      }
      if (ms_since(last_check) > 20000) {  // watchdog: the NCP must still answer
        last_check = Clock::now();
        uint8_t st;
        ok = d.net_state(st);
        if (!ok) LOGW("zigbee: watchdog: the NCP did not answer the network state request");
        if (ok) {
          d.joined = st == 2;
          set_status(d.idle_status());
        }
      }
    }
    if (!stop_) {
      LOGW("zigbee: radio link lost, restarting it");
      d.close_tty();
    }
  }
}

}  // namespace cs
