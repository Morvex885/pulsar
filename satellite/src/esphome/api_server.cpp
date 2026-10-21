#include "esphome/api_server.h"

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
#include "esphome/mdns.h"
#include "esphome/proto.h"

namespace cs {

namespace {

// API version announced in HelloResponse. 1.15 = DeviceCapabilitiesRequest/Response supported.
constexpr uint32_t kApiMajor = 1;
constexpr uint32_t kApiMinor = 15;
constexpr const char *kEsphomeVersion = "2026.9.0";
constexpr const char *kProjectName = "cucumber.satellite";

// Message ids (ref/api.proto `option (id)`).
enum Msg : uint32_t {
  HELLO_REQ = 1,
  HELLO_RESP = 2,
  DISCONNECT_REQ = 5,
  DISCONNECT_RESP = 6,
  PING_REQ = 7,
  PING_RESP = 8,
  DEVICE_INFO_REQ = 9,
  DEVICE_INFO_RESP = 10,
  LIST_ENTITIES_REQ = 11,
  LIST_BINARY_SENSOR = 12,
  LIST_LIGHT = 15,
  LIST_SENSOR = 16,
  LIST_SWITCH = 17,
  LIST_DONE = 19,
  SUBSCRIBE_STATES_REQ = 20,
  BINARY_SENSOR_STATE = 21,
  LIGHT_STATE = 24,
  SENSOR_STATE = 25,
  SWITCH_STATE = 26,
  SUBSCRIBE_LOGS_REQ = 28,
  LIGHT_CMD = 32,
  SWITCH_CMD = 33,
  SUBSCRIBE_HA_SERVICES_REQ = 34,
  GET_TIME_REQ = 36,
  GET_TIME_RESP = 37,
  SUBSCRIBE_HA_STATES_REQ = 38,
  LIST_NUMBER = 49,
  NUMBER_STATE = 50,
  NUMBER_CMD = 51,
  LIST_MEDIA_PLAYER = 63,
  MEDIA_PLAYER_STATE = 64,
  MEDIA_PLAYER_CMD = 65,
  SUBSCRIBE_VA_REQ = 89,
  VA_REQUEST = 90,
  VA_RESPONSE = 91,
  VA_EVENT_RESP = 92,
  VA_AUDIO = 106,
  LIST_EVENT = 107,
  EVENT_RESP = 108,
  VA_TIMER_EVENT_RESP = 115,
  VA_ANNOUNCE_REQ = 119,
  VA_ANNOUNCE_FINISHED = 120,
  VA_CONFIG_REQ = 121,
  VA_CONFIG_RESP = 122,
  VA_SET_CONFIG = 123,
  DEVICE_CAPS_REQ = 149,
  DEVICE_CAPS_RESP = 150,
};

// Entity keys (fixed32), stable across restarts.
enum Key : uint32_t {
  K_MEDIA = 1,
  K_SW_MIC_MUTE = 2,
  K_SW_CLOCK = 3,
  K_BS_HW_MIC_MUTE = 4,
  K_SENSOR_LUX = 5,
  K_NUMBER_CLOCK_BRI = 6,
  K_LIGHT_RING = 7,
  K_EVENT_BUTTON = 8,
};

// aioesphomeapi VoiceAssistantFeature bits.
constexpr uint32_t VAF_VOICE_ASSISTANT = 1, VAF_API_AUDIO = 4, VAF_TIMERS = 8, VAF_ANNOUNCE = 16,
                   VAF_START_CONVERSATION = 32;
constexpr uint32_t kVoiceFeatures =
    VAF_VOICE_ASSISTANT | VAF_API_AUDIO | VAF_TIMERS | VAF_ANNOUNCE | VAF_START_CONVERSATION;

// Home Assistant MediaPlayerEntityFeature bits.
constexpr uint32_t MPF_PAUSE = 1, MPF_VOLUME_SET = 4, MPF_VOLUME_MUTE = 8, MPF_PLAY_MEDIA = 512,
                   MPF_VOLUME_STEP = 1024, MPF_STOP = 4096, MPF_PLAY = 16384, MPF_MEDIA_ANNOUNCE = 1048576;
constexpr uint32_t kMediaFeatures = MPF_PAUSE | MPF_VOLUME_SET | MPF_VOLUME_MUTE | MPF_PLAY_MEDIA | MPF_VOLUME_STEP |
                                    MPF_STOP | MPF_PLAY | MPF_MEDIA_ANNOUNCE;

constexpr uint32_t ENTITY_CATEGORY_CONFIG = 1, ENTITY_CATEGORY_DIAGNOSTIC = 2;
constexpr uint32_t COLOR_MODE_RGB = 35;
constexpr uint32_t STATE_CLASS_MEASUREMENT = 1;
constexpr uint32_t NUMBER_MODE_SLIDER = 2;
constexpr uint32_t FORMAT_PURPOSE_DEFAULT = 0, FORMAT_PURPOSE_ANNOUNCEMENT = 1;

constexpr size_t kMaxFrame = 1 << 20;            // reject absurd frames
constexpr size_t kAudioBacklogLimit = 256 << 10;  // drop mic audio when the socket is this far behind
constexpr size_t kMaxBacklog = 8 << 20;           // give up on a client that stopped reading
constexpr int64_t kHelloTimeoutMs = 15000;
constexpr int64_t kIdlePingMs = 45000;
constexpr int64_t kIdleDropMs = 100000;
constexpr int64_t kTimeResyncMs = 6LL * 3600 * 1000;

struct BoolState {
  bool has = false;
  bool v = false;
};
struct FloatState {
  bool has = false;
  float v = 0;
};

float clamp01(float v) { return v < 0 ? 0 : (v > 1 ? 1 : v); }

// POSIX TZ string for a ParsedTimezone (GetTimeResponse field 3) without DST rules, e.g. "<+0300>-3".
// std_offset_seconds uses the POSIX sign convention (seconds west of UTC: MSK-3 -> -10800).
// Returns "" when the zone has DST (not trivially representable here).
std::string posix_tz_from_parsed(const pb::Field &pf) {
  int32_t std_off = 0;
  bool has_dst = false;
  pb::Reader r(pf);
  pb::Field f;
  while (r.next(f)) {
    if (f.num == 1) std_off = int32_t(uint32_t(f.v >> 1) ^ -uint32_t(f.v & 1));  // sint32 zigzag
    if (f.num == 3 || f.num == 4) {
      pb::Reader rr(f);
      pb::Field rf;
      while (rr.next(rf))
        if (rf.num == 3 && rf.v != 0) has_dst = true;  // DSTRule.type != NONE
    }
  }
  if (has_dst || r.error()) return {};
  int32_t east = -std_off;
  uint32_t a = uint32_t(east < 0 ? -east : east);
  char name[16], off[24];
  snprintf(name, sizeof name, "<%c%02u%02u>", east < 0 ? '-' : '+', a / 3600, a / 60 % 60);
  // POSIX offset: west-positive hours[:mm[:ss]].
  uint32_t w = uint32_t(std_off < 0 ? -std_off : std_off);
  if (w % 60)
    snprintf(off, sizeof off, "%s%u:%02u:%02u", std_off < 0 ? "-" : "", w / 3600, w / 60 % 60, w % 60);
  else if (w % 3600)
    snprintf(off, sizeof off, "%s%u:%02u", std_off < 0 ? "-" : "", w / 3600, w / 60 % 60);
  else
    snprintf(off, sizeof off, "%s%u", std_off < 0 ? "-" : "", w / 3600);
  return std::string(name) + off;
}

}  // namespace

struct ApiServer::Impl {
  struct Client {
    int fd = -1;
    std::string peer;
    std::string rbuf;
    std::string wbuf;
    size_t woff = 0;
    bool want_out = false;
    bool hello = false;
    bool states = false;
    bool voice = false;
    bool close_after_flush = false;
    int64_t connected_at = 0;
    int64_t last_rx = 0;
    bool ping_outstanding = false;
    int64_t last_time_req = 0;
  };

  Loop &loop;
  DeviceIdentity id;
  ApiConfig cfg;
  ApiHooks &hooks;
  int listen_fd = -1;
  std::shared_ptr<Client> client;
  // A new connection while a client is established stays pending until it sends a valid
  // HelloRequest; only then does it replace the established client. Garbage/port probes
  // therefore cannot knock Home Assistant off.
  std::shared_ptr<Client> pending;
  std::unique_ptr<MdnsResponder> mdns;
  uint64_t tick_timer = 0;
  bool audio_drop_logged = false;

  // Cached entity state (replayed on SubscribeStatesRequest).
  MediaState media_state = MediaState::IDLE;
  float media_volume = 0;
  bool media_muted = false;
  BoolState sw_mic_mute, sw_clock, bs_hw_mute;
  FloatState lux, clock_bri;
  LightState light;

  Impl(Loop &l, DeviceIdentity i, ApiConfig c, ApiHooks &h) : loop(l), id(std::move(i)), cfg(std::move(c)), hooks(h) {}

  ~Impl() {
    if (tick_timer) loop.cancel_timer(tick_timer);
    for (auto *slot : {&client, &pending}) {
      if (*slot) {
        loop.del_fd((*slot)->fd);
        close((*slot)->fd);
        slot->reset();
      }
    }
    if (listen_fd >= 0) {
      loop.del_fd(listen_fd);
      close(listen_fd);
    }
  }

  bool ready() const { return client && client->hello; }

  // ---------------------------------------------------------------- transport

  bool start() {
    listen_fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (listen_fd < 0) {
      LOGE("api: socket: %s", strerror(errno));
      return false;
    }
    int one = 1;
    setsockopt(listen_fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    sockaddr_in sa{};
    sa.sin_family = AF_INET;
    sa.sin_port = htons(cfg.port);
    sa.sin_addr.s_addr = htonl(INADDR_ANY);
    if (bind(listen_fd, reinterpret_cast<sockaddr *>(&sa), sizeof sa) != 0 || listen(listen_fd, 4) != 0) {
      LOGE("api: bind/listen :%u: %s", cfg.port, strerror(errno));
      close(listen_fd);
      listen_fd = -1;
      return false;
    }
    loop.add_fd(listen_fd, EPOLLIN, [this](uint32_t) { on_accept(); });
    tick_timer = loop.add_timer(5000, [this] { tick(); }, 5000);
    LOGI("api: listening on :%u as '%s'", cfg.port, id.name.c_str());

    std::string mac_plain;
    for (char c : id.mac)
      if (c != ':') mac_plain.push_back(char(c >= 'A' && c <= 'F' ? c - 'A' + 'a' : c));
    std::vector<std::string> txt = {
        "friendly_name=" + id.friendly_name,
        std::string("version=") + kEsphomeVersion,
        "mac=" + mac_plain,
        "platform=host",
        "board=" + id.board,
        "network=wifi",
        std::string("project_name=") + kProjectName,
        "project_version=" + id.project_version,
    };
    mdns.reset(new MdnsResponder(loop, id.name, "_esphomelib._tcp", cfg.port, std::move(txt)));
    if (!mdns->start()) LOGW("api: mDNS advertising unavailable");
    return true;
  }

  void on_accept() {
    for (;;) {
      sockaddr_in sa{};
      socklen_t sl = sizeof sa;
      int fd = accept4(listen_fd, reinterpret_cast<sockaddr *>(&sa), &sl, SOCK_NONBLOCK | SOCK_CLOEXEC);
      if (fd < 0) {
        if (errno == EINTR) continue;
        if (errno != EAGAIN && errno != EWOULDBLOCK) LOGW("api: accept: %s", strerror(errno));
        return;
      }
      if (pending) close_pending("superseded by another connection");
      int one = 1;
      setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
      setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, &one, sizeof one);
      char ip[INET_ADDRSTRLEN];
      inet_ntop(AF_INET, &sa.sin_addr, ip, sizeof ip);
      auto c = std::make_shared<Client>();
      c->fd = fd;
      c->peer = std::string(ip) + ":" + std::to_string(ntohs(sa.sin_port));
      c->connected_at = c->last_rx = Loop::now_ms();
      if (client) {
        pending = c;
        LOGI("api: connection from %s (pending until hello)", c->peer.c_str());
      } else {
        client = c;
        LOGI("api: connection from %s", c->peer.c_str());
      }
      loop.add_fd(fd, EPOLLIN | EPOLLRDHUP, [this, c](uint32_t ev) { on_client_event(c, ev); });
    }
  }

  void close_client(const char *why) {
    if (!client) return;
    std::shared_ptr<Client> c = std::move(client);
    client.reset();
    loop.del_fd(c->fd);
    close(c->fd);
    LOGI("api: %s disconnected (%s)", c->peer.c_str(), why);
    if (c->hello) {
      if (c->voice) hooks.on_voice_subscribed(false);
      hooks.on_client_disconnected();
    }
  }

  void close_pending(const char *why) {
    if (!pending) return;
    std::shared_ptr<Client> c = std::move(pending);
    pending.reset();
    loop.del_fd(c->fd);
    close(c->fd);
    LOGI("api: %s dropped (%s)", c->peer.c_str(), why);
  }

  void on_pending_event(const std::shared_ptr<Client> &c) {
    bool eof = read_into(c);
    const auto *p = reinterpret_cast<const uint8_t *>(c->rbuf.data());
    if (!c->rbuf.empty()) {
      if (p[0] != 0x00) return close_pending(p[0] == 0x01 ? "noise encryption requested (unsupported)" : "bad frame preamble");
      size_t q = 1;
      uint64_t len, type;
      if (pb::get_varint(p, c->rbuf.size(), &q, &len) && pb::get_varint(p, c->rbuf.size(), &q, &type)) {
        if (type != HELLO_REQ) return close_pending("message before hello");
        if (len > kMaxFrame) return close_pending("frame too large");
        if (c->rbuf.size() - q >= len) {  // complete HelloRequest: promote
          std::shared_ptr<Client> keep = c;
          pending.reset();
          close_client("replaced by a new connection");
          client = keep;
          LOGI("api: %s took over the API connection", keep->peer.c_str());
          process(keep, eof);
          return;
        }
      } else if (c->rbuf.size() > 21) {
        return close_pending("malformed frame header");
      }
    }
    if (eof) close_pending("closed before hello");
  }

  void on_client_event(const std::shared_ptr<Client> &c, uint32_t ev) {
    if (c == pending) return on_pending_event(c);
    if (client != c) return;
    if (ev & EPOLLOUT) {
      flush();
      if (client != c) return;
    }
    if (ev & (EPOLLIN | EPOLLRDHUP | EPOLLHUP | EPOLLERR)) on_readable(c);
  }

  void on_readable(const std::shared_ptr<Client> &c) { process(c, read_into(c)); }

  // Reads everything available into c->rbuf; returns true on EOF/error.
  bool read_into(const std::shared_ptr<Client> &c) {
    char buf[16384];
    bool eof = false;
    for (;;) {
      ssize_t n = recv(c->fd, buf, sizeof buf, 0);
      if (n > 0) {
        c->rbuf.append(buf, size_t(n));
        continue;
      }
      if (n == 0) {
        eof = true;
      } else if (errno == EINTR) {
        continue;
      } else if (errno != EAGAIN && errno != EWOULDBLOCK) {
        LOGW("api: recv: %s", strerror(errno));
        eof = true;
      }
      break;
    }
    if (!c->rbuf.empty()) {
      c->last_rx = Loop::now_ms();
      c->ping_outstanding = false;
    }
    return eof;
  }

  void process(const std::shared_ptr<Client> &c, bool eof) {
    size_t pos = 0;
    const auto *p = reinterpret_cast<const uint8_t *>(c->rbuf.data());
    while (client == c && pos < c->rbuf.size()) {
      size_t avail = c->rbuf.size() - pos;
      if (p[pos] != 0x00) {
        close_client(p[pos] == 0x01 ? "client requested noise encryption (unsupported)" : "bad frame preamble");
        return;
      }
      size_t q = pos + 1;
      uint64_t len, type;
      if (!pb::get_varint(p, c->rbuf.size(), &q, &len) || !pb::get_varint(p, c->rbuf.size(), &q, &type)) {
        if (avail > 21) {
          close_client("malformed frame header");
          return;
        }
        break;  // header incomplete
      }
      if (len > kMaxFrame) {
        close_client("frame too large");
        return;
      }
      if (c->rbuf.size() - q < len) break;  // payload incomplete
      handle(c, uint32_t(type), p + q, size_t(len));
      pos = q + size_t(len);
    }
    if (client != c) return;
    c->rbuf.erase(0, pos);
    if (eof) close_client("connection closed by peer");
  }

  void send_msg(uint32_t type, const pb::Writer &w) {
    if (!client) return;
    Client &c = *client;
    if (c.woff && c.woff == c.wbuf.size()) {
      c.wbuf.clear();
      c.woff = 0;
    }
    c.wbuf.push_back(0);
    pb::put_varint(c.wbuf, w.size());
    pb::put_varint(c.wbuf, type);
    c.wbuf += w.data();
    flush();
  }

  void flush() {
    if (!client) return;
    Client &c = *client;
    while (c.woff < c.wbuf.size()) {
      ssize_t n = send(c.fd, c.wbuf.data() + c.woff, c.wbuf.size() - c.woff, MSG_NOSIGNAL);
      if (n > 0) {
        c.woff += size_t(n);
        continue;
      }
      if (n < 0 && errno == EINTR) continue;
      if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
        if (c.wbuf.size() - c.woff > kMaxBacklog) {
          close_client("client not reading");
          return;
        }
        if (c.woff > (64u << 10) && c.woff * 2 > c.wbuf.size()) {
          c.wbuf.erase(0, c.woff);
          c.woff = 0;
        }
        if (!c.want_out) {
          c.want_out = true;
          loop.mod_fd(c.fd, EPOLLIN | EPOLLRDHUP | EPOLLOUT);
        }
        return;
      }
      LOGW("api: send: %s", n < 0 ? strerror(errno) : "0 bytes");
      close_client("write error");
      return;
    }
    c.wbuf.clear();
    c.woff = 0;
    if (c.want_out) {
      c.want_out = false;
      loop.mod_fd(c.fd, EPOLLIN | EPOLLRDHUP);
    }
    if (c.close_after_flush) close_client("disconnect requested");
  }

  size_t backlog() const { return client ? client->wbuf.size() - client->woff : 0; }

  void tick() {
    if (pending && Loop::now_ms() - pending->connected_at > kHelloTimeoutMs) close_pending("no hello");
    if (!client) return;
    int64_t now = Loop::now_ms();
    if (!client->hello) {
      if (now - client->connected_at > kHelloTimeoutMs) close_client("no hello");
      return;
    }
    int64_t idle = now - client->last_rx;
    if (idle > kIdleDropMs) {
      close_client("keepalive timeout");
      return;
    }
    if (idle > kIdlePingMs && !client->ping_outstanding) {
      client->ping_outstanding = true;
      send_msg(PING_REQ, pb::Writer());
    }
    if (client && now - client->last_time_req >= kTimeResyncMs) request_time();
  }

  void request_time() {
    if (!ready()) return;
    client->last_time_req = Loop::now_ms();
    send_msg(GET_TIME_REQ, pb::Writer());
  }

  // ---------------------------------------------------------------- dispatch

  void handle(const std::shared_ptr<Client> &c, uint32_t type, const uint8_t *p, size_t n) {
    pb::Reader r(p, n);
    pb::Field f;
    if (!c->hello && type != HELLO_REQ && type != DISCONNECT_REQ && type != PING_REQ) {
      LOGW("api: message %u before hello", type);
      close_client("protocol violation");
      return;
    }
    switch (type) {
      case HELLO_REQ: {
        std::string info;
        uint32_t major = 0, minor = 0;
        while (r.next(f)) {
          if (f.num == 1) info = f.str();
          if (f.num == 2) major = f.u32();
          if (f.num == 3) minor = f.u32();
        }
        pb::Writer w;
        w.u32(1, kApiMajor);
        w.u32(2, kApiMinor);
        w.str(3, std::string("cucumber-satellite ") + id.project_version, true);
        w.str(4, id.name, true);
        send_msg(HELLO_RESP, w);
        if (client != c) return;
        LOGI("api: hello from '%s' (api %u.%u)", info.c_str(), major, minor);
        if (!c->hello) {
          c->hello = true;
          hooks.on_client_connected(info);
          if (client == c) request_time();
        }
        break;
      }
      case DISCONNECT_REQ:
        c->close_after_flush = true;
        send_msg(DISCONNECT_RESP, pb::Writer());
        break;
      case DISCONNECT_RESP:
        close_client("disconnect acknowledged");
        break;
      case PING_REQ:
        send_msg(PING_RESP, pb::Writer());
        break;
      case PING_RESP:
        break;
      case DEVICE_INFO_REQ:
        send_device_info();
        break;
      case DEVICE_CAPS_REQ: {
        pb::Writer va, w;
        va.u32(1, kVoiceFeatures);
        w.msg(2, va);
        send_msg(DEVICE_CAPS_RESP, w);
        break;
      }
      case LIST_ENTITIES_REQ:
        list_entities();
        break;
      case SUBSCRIBE_STATES_REQ:
        c->states = true;
        send_all_states();
        break;
      case SUBSCRIBE_LOGS_REQ:
      case SUBSCRIBE_HA_SERVICES_REQ:
      case SUBSCRIBE_HA_STATES_REQ:
        break;
      case GET_TIME_RESP: {
        uint32_t epoch = 0;
        std::string tz;
        pb::Field parsed;
        bool has_parsed = false;
        while (r.next(f)) {
          if (f.num == 1) epoch = f.u32();
          if (f.num == 2) tz = f.str();
          if (f.num == 3 && f.wt == pb::LEN) parsed = f, has_parsed = true;
        }
        if (tz.empty() && has_parsed) tz = posix_tz_from_parsed(parsed);
        LOGI("api: time from HA: epoch=%u tz='%s'", epoch, tz.c_str());
        if (epoch) hooks.on_time(epoch, tz);
        break;
      }
      case MEDIA_PLAYER_CMD:
        on_media_cmd(r);
        break;
      case SWITCH_CMD:
        on_switch_cmd(r);
        break;
      case NUMBER_CMD:
        on_number_cmd(r);
        break;
      case LIGHT_CMD:
        on_light_cmd(r);
        break;
      case SUBSCRIBE_VA_REQ: {
        bool sub = false;
        uint32_t flags = 0;
        while (r.next(f)) {
          if (f.num == 1) sub = f.boolean();
          if (f.num == 2) flags = f.u32();
        }
        LOGI("api: voice assistant %s (flags=%u)", sub ? "subscribed" : "unsubscribed", flags);
        if (c->voice != sub) {
          c->voice = sub;
          hooks.on_voice_subscribed(sub);
        }
        break;
      }
      case VA_RESPONSE: {
        uint32_t port = 0;
        bool error = false;
        while (r.next(f)) {
          if (f.num == 1) port = f.u32();
          if (f.num == 2) error = f.boolean();
        }
        if (error)
          LOGW("api: HA failed to start the voice pipeline");
        else
          LOGI("api: voice pipeline started (%s audio)", port ? "UDP" : "API");
        break;
      }
      case VA_EVENT_RESP: {
        uint32_t type_v = 0;
        std::map<std::string, std::string> data;
        while (r.next(f)) {
          if (f.num == 1) type_v = f.u32();
          if (f.num == 2 && f.wt == pb::LEN) {
            pb::Reader sr(f);
            pb::Field sf;
            std::string k, v;
            while (sr.next(sf)) {
              if (sf.num == 1) k = sf.str();
              if (sf.num == 2) v = sf.str();
            }
            data[k] = v;
          }
        }
        LOGD("api: voice event %u", type_v);
        hooks.on_voice_event(VaEvent(type_v), data);
        break;
      }
      case VA_TIMER_EVENT_RESP: {
        TimerInfo t{};
        t.event = TimerEvent::STARTED;
        while (r.next(f)) {
          switch (f.num) {
            case 1: t.event = TimerEvent(f.u32()); break;
            case 2: t.id = f.str(); break;
            case 3: t.name = f.str(); break;
            case 4: t.total_seconds = f.u32(); break;
            case 5: t.seconds_left = f.u32(); break;
            case 6: t.is_active = f.boolean(); break;
          }
        }
        hooks.on_timer_event(t);
        break;
      }
      case VA_ANNOUNCE_REQ: {
        AnnounceRequest a;
        while (r.next(f)) {
          switch (f.num) {
            case 1: a.media_id = f.str(); break;
            case 2: a.text = f.str(); break;
            case 3: a.preannounce_media_id = f.str(); break;
            case 4: a.start_conversation = f.boolean(); break;
          }
        }
        LOGI("api: announce '%s' media=%s", a.text.c_str(), a.media_id.c_str());
        hooks.on_announce(a);
        break;
      }
      case VA_CONFIG_REQ:
        send_va_config();
        break;
      case VA_SET_CONFIG: {
        std::vector<std::string> ids;
        while (r.next(f))
          if (f.num == 1 && f.wt == pb::LEN) {
            std::string ww = f.str();
            bool known = std::any_of(cfg.wake_words.begin(), cfg.wake_words.end(),
                                     [&](const WakeWordInfo &w) { return w.id == ww; });
            if (known && ids.empty())  // max_active_wake_words = 1
              ids.push_back(ww);
            else
              LOGW("api: ignoring wake word '%s'", ww.c_str());
          }
        cfg.active_wake_words = ids;
        hooks.on_active_wake_words(ids);
        break;
      }
      case VA_AUDIO:
        break;  // HA->device TTS audio over API; we do not advertise SPEAKER
      default:
        LOGD("api: ignoring message type %u (%zu bytes)", type, n);
        break;
    }
    if (r.error()) LOGW("api: malformed payload in message %u", type);
  }

  // ---------------------------------------------------------------- device info / entities

  void send_device_info() {
    pb::Writer w;
    w.str(2, id.name, true);
    w.str(3, id.mac, true);
    w.str(4, kEsphomeVersion, true);
    w.str(5, __DATE__ ", " __TIME__, true);
    w.str(6, id.model, true);
    w.str(8, kProjectName, true);
    w.str(9, id.project_version, true);
    w.str(12, id.manufacturer, true);
    w.str(13, id.friendly_name, true);
    w.u32(17, kVoiceFeatures);
    send_msg(DEVICE_INFO_RESP, w);
  }

  static void entity_head(pb::Writer &w, const char *object_id, uint32_t key, const char *name) {
    w.str(1, object_id, true);
    w.fixed32(2, key, true);
    w.str(3, name, true);
  }

  void list_entities() {
    {
      pb::Writer w;
      entity_head(w, "media_player", K_MEDIA, "Media Player");
      w.boolean(8, true);  // supports_pause (deprecated, for older clients)
      auto fmt = [&](uint32_t channels, uint32_t purpose) {
        pb::Writer s;
        s.str(1, "flac");
        s.u32(2, 48000);
        s.u32(3, channels);
        s.u32(4, purpose);
        s.u32(5, 2);  // 16-bit
        w.msg(9, s);
      };
      fmt(2, FORMAT_PURPOSE_DEFAULT);
      fmt(1, FORMAT_PURPOSE_ANNOUNCEMENT);
      w.u32(11, kMediaFeatures);
      send_msg(LIST_MEDIA_PLAYER, w);
    }
    auto sw = [&](const char *oid, uint32_t key, const char *name, const char *icon) {
      pb::Writer w;
      entity_head(w, oid, key, name);
      w.str(5, icon);
      send_msg(LIST_SWITCH, w);
    };
    sw("mute_microphones", K_SW_MIC_MUTE, "Mute microphones", "mdi:microphone-off");
    sw("clock_display", K_SW_CLOCK, "Clock display", "mdi:clock-digital");
    {
      pb::Writer w;
      entity_head(w, "hardware_mic_mute", K_BS_HW_MIC_MUTE, "Hardware mic mute");
      w.str(8, "mdi:microphone-off");
      w.u32(9, ENTITY_CATEGORY_DIAGNOSTIC);
      send_msg(LIST_BINARY_SENSOR, w);
    }
    {
      pb::Writer w;
      entity_head(w, "illuminance", K_SENSOR_LUX, "Illuminance");
      w.str(6, "lx");
      w.i32(7, 0);
      w.str(9, "illuminance");
      w.u32(10, STATE_CLASS_MEASUREMENT);
      send_msg(LIST_SENSOR, w);
    }
    {
      pb::Writer w;
      entity_head(w, "clock_brightness", K_NUMBER_CLOCK_BRI, "Clock brightness");
      w.str(5, "mdi:brightness-6");
      w.flt(6, 0);
      w.flt(7, 100);
      w.flt(8, 1);
      w.u32(10, ENTITY_CATEGORY_CONFIG);
      w.str(11, "%");
      w.u32(12, NUMBER_MODE_SLIDER);
      send_msg(LIST_NUMBER, w);
    }
    {
      pb::Writer w;
      entity_head(w, "ring", K_LIGHT_RING, "Ring");
      w.str(11, "None", true);
      for (const auto &e : cfg.light_effects) w.str(11, e, true);
      w.packed_u32(12, {COLOR_MODE_RGB});
      w.str(14, "mdi:led-strip-variant");
      send_msg(LIST_LIGHT, w);
    }
    {
      pb::Writer w;
      entity_head(w, "button", K_EVENT_BUTTON, "Button");
      w.str(5, "mdi:gesture-tap-button");
      w.str(8, "button");
      for (const char *t : {"action", "action_long", "volume_up", "volume_down"}) w.str(9, t, true);
      send_msg(LIST_EVENT, w);
    }
    send_msg(LIST_DONE, pb::Writer());
  }

  // ---------------------------------------------------------------- states

  bool states_on() const { return client && client->hello && client->states; }

  void send_media() {
    pb::Writer w;
    w.fixed32(1, K_MEDIA, true);
    w.u32(2, uint32_t(media_state));
    w.flt(3, media_volume);
    w.boolean(4, media_muted);
    send_msg(MEDIA_PLAYER_STATE, w);
  }
  void send_switch(uint32_t key, const BoolState &s) {
    pb::Writer w;
    w.fixed32(1, key, true);
    w.boolean(2, s.v);
    w.boolean(4, !s.has);  // missing_state
    send_msg(SWITCH_STATE, w);
  }
  void send_binary(uint32_t key, const BoolState &s) {
    pb::Writer w;
    w.fixed32(1, key, true);
    w.boolean(2, s.v);
    w.boolean(3, !s.has);
    send_msg(BINARY_SENSOR_STATE, w);
  }
  void send_float(uint32_t type, uint32_t key, const FloatState &s) {
    pb::Writer w;
    w.fixed32(1, key, true);
    w.flt(2, s.v);
    w.boolean(3, !s.has);
    send_msg(type, w);
  }
  void send_light() {
    // HA expects rgb normalised to max 1 with the scale carried in color_brightness.
    float m = std::max({light.r, light.g, light.b});
    pb::Writer w;
    w.fixed32(1, K_LIGHT_RING, true);
    w.boolean(2, light.on);
    w.flt(3, light.brightness);
    w.flt(10, m > 0 ? m : 1.0f);
    w.u32(11, COLOR_MODE_RGB);
    w.flt(4, m > 0 ? light.r / m : 1.0f);
    w.flt(5, m > 0 ? light.g / m : 1.0f);
    w.flt(6, m > 0 ? light.b / m : 1.0f);
    w.str(9, light.effect.empty() ? std::string("None") : light.effect);
    send_msg(LIGHT_STATE, w);
  }

  void send_all_states() {
    send_media();
    send_switch(K_SW_MIC_MUTE, sw_mic_mute);
    send_switch(K_SW_CLOCK, sw_clock);
    send_binary(K_BS_HW_MIC_MUTE, bs_hw_mute);
    send_float(SENSOR_STATE, K_SENSOR_LUX, lux);
    send_float(NUMBER_STATE, K_NUMBER_CLOCK_BRI, clock_bri);
    send_light();
  }

  // ---------------------------------------------------------------- commands

  void on_media_cmd(pb::Reader &r) {
    MediaCommandRequest cmd;
    uint32_t key = 0;
    pb::Field f;
    while (r.next(f)) {
      switch (f.num) {
        case 1: key = f.u32(); break;
        case 2: cmd.has_command = f.boolean(); break;
        case 3: cmd.command = MediaCommand(f.u32()); break;
        case 4: cmd.has_volume = f.boolean(); break;
        case 5: cmd.volume = f.flt(); break;
        case 6: cmd.has_media_url = f.boolean(); break;
        case 7: cmd.media_url = f.str(); break;
        case 8: cmd.has_announcement = f.boolean(); break;
        case 9: cmd.announcement = f.boolean(); break;
      }
    }
    if (key != K_MEDIA) return;
    if (cmd.has_volume) cmd.volume = clamp01(cmd.volume);
    hooks.on_media_command(cmd);
  }

  void on_switch_cmd(pb::Reader &r) {
    uint32_t key = 0;
    bool state = false;
    pb::Field f;
    while (r.next(f)) {
      if (f.num == 1) key = f.u32();
      if (f.num == 2) state = f.boolean();
    }
    if (key == K_SW_MIC_MUTE)
      hooks.on_switch(SwitchId::MIC_MUTE, state);
    else if (key == K_SW_CLOCK)
      hooks.on_switch(SwitchId::CLOCK, state);
  }

  void on_number_cmd(pb::Reader &r) {
    uint32_t key = 0;
    float v = 0;
    pb::Field f;
    while (r.next(f)) {
      if (f.num == 1) key = f.u32();
      if (f.num == 2) v = f.flt();
    }
    if (key != K_NUMBER_CLOCK_BRI || v != v) return;
    hooks.on_number(NumberId::CLOCK_BRIGHTNESS, std::min(100.0f, std::max(0.0f, v)));
  }

  void on_light_cmd(pb::Reader &r) {
    uint32_t key = 0;
    bool has_state = false, state = false, has_bri = false, has_cbri = false, has_rgb = false, has_effect = false;
    float bri = 0, cbri = 1, red = 0, green = 0, blue = 0;
    std::string effect;
    pb::Field f;
    while (r.next(f)) {
      switch (f.num) {
        case 1: key = f.u32(); break;
        case 2: has_state = f.boolean(); break;
        case 3: state = f.boolean(); break;
        case 4: has_bri = f.boolean(); break;
        case 5: bri = f.flt(); break;
        case 20: has_cbri = f.boolean(); break;
        case 21: cbri = f.flt(); break;
        case 6: has_rgb = f.boolean(); break;
        case 7: red = f.flt(); break;
        case 8: green = f.flt(); break;
        case 9: blue = f.flt(); break;
        case 18: has_effect = f.boolean(); break;
        case 19: effect = f.str(); break;
      }
    }
    if (key != K_LIGHT_RING) return;
    LightState st = light;
    if (has_state) st.on = state;
    if (has_bri) st.brightness = clamp01(bri);
    if (has_rgb) {
      float scale = has_cbri ? clamp01(cbri) : 1.0f;
      st.r = clamp01(red) * scale;
      st.g = clamp01(green) * scale;
      st.b = clamp01(blue) * scale;
    } else if (has_cbri) {
      float m = std::max({st.r, st.g, st.b});
      float scale = m > 0 ? clamp01(cbri) / m : 0;
      st.r *= scale;
      st.g *= scale;
      st.b *= scale;
    }
    if (has_effect) st.effect = effect == "None" ? std::string() : effect;
    hooks.on_light(st);
    light = st;
    if (states_on()) send_light();
  }

  void send_va_config() {
    pb::Writer w;
    for (const auto &ww : cfg.wake_words) {
      pb::Writer s;
      s.str(1, ww.id);
      s.str(2, ww.phrase);
      for (const auto &l : ww.languages) s.str(3, l, true);
      w.msg(1, s);
    }
    for (const auto &a : cfg.active_wake_words) w.str(2, a, true);
    w.u32(3, 1);
    send_msg(VA_CONFIG_RESP, w);
  }
};

// ------------------------------------------------------------------ public API

ApiServer::ApiServer(Loop &loop, DeviceIdentity id, ApiConfig cfg, ApiHooks &hooks)
    : impl_(new Impl(loop, std::move(id), std::move(cfg), hooks)) {}

ApiServer::~ApiServer() = default;

bool ApiServer::start() { return impl_->start(); }

bool ApiServer::connected() const { return impl_->ready(); }

bool ApiServer::voice_subscribed() const { return impl_->ready() && impl_->client->voice; }
MdnsResponder *ApiServer::mdns() { return impl_->mdns.get(); }

void ApiServer::voice_request_start(const std::string &wake_word_phrase) {
  if (!voice_subscribed()) {
    LOGW("api: voice_request_start without a subscribed client");
    return;
  }
  pb::Writer audio, w;
  audio.u32(1, 0);       // noise_suppression_level
  audio.u32(2, 0);       // auto_gain
  audio.flt(3, 1.0f);    // volume_multiplier
  w.boolean(1, true);    // start
  w.u32(3, 0);           // flags: wake word handled on-device, VAD in HA
  w.msg(4, audio);
  w.str(5, wake_word_phrase);
  impl_->audio_drop_logged = false;
  impl_->send_msg(VA_REQUEST, w);
}

void ApiServer::voice_request_stop() {
  if (!voice_subscribed()) return;
  pb::Writer w;
  w.boolean(1, false);
  impl_->send_msg(VA_REQUEST, w);
}

void ApiServer::voice_send_audio(const int16_t *pcm, size_t samples) {
  if (!voice_subscribed() || !samples) return;
  if (impl_->backlog() > kAudioBacklogLimit) {
    if (!impl_->audio_drop_logged) LOGW("api: network backlog, dropping mic audio");
    impl_->audio_drop_logged = true;
    return;
  }
  pb::Writer w;
  w.bytes(1, pcm, samples * sizeof(int16_t));
  impl_->send_msg(VA_AUDIO, w);
}

void ApiServer::voice_announce_finished(bool success) {
  if (!voice_subscribed()) return;
  pb::Writer w;
  w.boolean(1, success);
  impl_->send_msg(VA_ANNOUNCE_FINISHED, w);
}

void ApiServer::set_active_wake_words(const std::vector<std::string> &ids) { impl_->cfg.active_wake_words = ids; }

void ApiServer::publish_media(MediaState st, float volume, bool muted) {
  impl_->media_state = st;
  impl_->media_volume = clamp01(volume);
  impl_->media_muted = muted;
  if (impl_->states_on()) impl_->send_media();
}

void ApiServer::publish_switch(SwitchId id, bool state) {
  bool clock = id == SwitchId::CLOCK;
  BoolState &s = clock ? impl_->sw_clock : impl_->sw_mic_mute;
  s.has = true;
  s.v = state;
  if (impl_->states_on()) impl_->send_switch(clock ? K_SW_CLOCK : K_SW_MIC_MUTE, s);
}

void ApiServer::publish_binary_sensor(BinarySensorId, bool state) {
  impl_->bs_hw_mute.has = true;
  impl_->bs_hw_mute.v = state;
  if (impl_->states_on()) impl_->send_binary(K_BS_HW_MIC_MUTE, impl_->bs_hw_mute);
}

void ApiServer::publish_sensor(SensorId, float value) {
  impl_->lux.has = true;
  impl_->lux.v = value;
  if (impl_->states_on()) impl_->send_float(SENSOR_STATE, K_SENSOR_LUX, impl_->lux);
}

void ApiServer::publish_number(NumberId, float value) {
  impl_->clock_bri.has = true;
  impl_->clock_bri.v = value;
  if (impl_->states_on()) impl_->send_float(NUMBER_STATE, K_NUMBER_CLOCK_BRI, impl_->clock_bri);
}

void ApiServer::publish_light(const LightState &st) {
  impl_->light = st;
  if (impl_->states_on()) impl_->send_light();
}

void ApiServer::fire_event(EventId, const std::string &event_type) {
  if (!impl_->states_on()) return;
  pb::Writer w;
  w.fixed32(1, K_EVENT_BUTTON, true);
  w.str(2, event_type, true);
  impl_->send_msg(EVENT_RESP, w);
}

}  // namespace cs
