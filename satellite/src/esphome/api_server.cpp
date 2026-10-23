#include "esphome/api_server.h"

#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <noise/protocol.h>
#include <sys/epoll.h>
#include <sys/random.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <array>
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
constexpr const char *kProjectName = "pulsar.satellite";

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
  LIST_TEXT_SENSOR = 18,
  LIST_DONE = 19,
  SUBSCRIBE_STATES_REQ = 20,
  BINARY_SENSOR_STATE = 21,
  LIGHT_STATE = 24,
  SENSOR_STATE = 25,
  SWITCH_STATE = 26,
  TEXT_SENSOR_STATE = 27,
  SUBSCRIBE_LOGS_REQ = 28,
  SUBSCRIBE_LOGS_RESP = 29,
  LIGHT_CMD = 32,
  SWITCH_CMD = 33,
  SUBSCRIBE_HA_SERVICES_REQ = 34,
  GET_TIME_REQ = 36,
  GET_TIME_RESP = 37,
  SUBSCRIBE_HA_STATES_REQ = 38,
  LIST_SERVICES = 41,
  EXECUTE_SERVICE_REQ = 42,
  LIST_NUMBER = 49,
  NUMBER_STATE = 50,
  NUMBER_CMD = 51,
  LIST_SELECT = 52,
  SELECT_STATE = 53,
  SELECT_CMD = 54,
  LIST_BUTTON = 61,
  BUTTON_CMD = 62,
  LIST_MEDIA_PLAYER = 63,
  MEDIA_PLAYER_STATE = 64,
  MEDIA_PLAYER_CMD = 65,
  SUBSCRIBE_VA_REQ = 89,
  VA_REQUEST = 90,
  VA_RESPONSE = 91,
  VA_EVENT_RESP = 92,
  LIST_TEXT = 97,
  TEXT_STATE = 98,
  TEXT_CMD = 99,
  VA_AUDIO = 106,
  LIST_EVENT = 107,
  EVENT_RESP = 108,
  VA_TIMER_EVENT_RESP = 115,
  LIST_UPDATE = 116,
  UPDATE_STATE = 117,
  UPDATE_CMD = 118,
  VA_ANNOUNCE_REQ = 119,
  VA_ANNOUNCE_FINISHED = 120,
  VA_CONFIG_REQ = 121,
  VA_CONFIG_RESP = 122,
  VA_SET_CONFIG = 123,
  NOISE_SET_KEY_REQ = 124,
  NOISE_SET_KEY_RESP = 125,
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
  K_BUTTON_RESTART = 9,
  K_SENSOR_RSSI = 10,
  K_SENSOR_UPTIME = 11,
  K_TEXT_IP = 12,
  K_TEXT_VERSION = 13,
  K_TEXT_CLOCK = 14,
  K_SELECT_CLOCK_FACE = 15,
  K_NUMBER_CLOCK_SCROLL = 16,
  K_ACTION_CLOCK_TEXT = 17,
  K_ACTION_CLOCK_FACE = 18,
  K_UPDATE = 19,
  K_SW_AUTO_UPDATE = 20,
  K_SELECT_ZIGBEE_MODE = 21,
  K_BUTTON_ZIGBEE_JOIN = 22,
  K_BUTTON_ZIGBEE_LEAVE = 23,
  K_TEXT_ZIGBEE = 24,
  K_SW_NEAREST_ANSWERS = 25,
  K_SELECT_MUSIC_VIZ = 26,
  K_NUMBER_LED_SPEED = 27,
  K_SW_VOICE_INDICATION = 28,
  K_SELECT_CASE_COLOR = 29,
  K_ACTION_LED_SHOW = 30,
  K_SELECT_CLOCK_MUSIC = 31,
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
constexpr uint32_t STATE_CLASS_MEASUREMENT = 1, STATE_CLASS_TOTAL_INCREASING = 2;
constexpr int64_t kDiagPeriodMs = 60000;  // Wi-Fi signal / uptime / IP refresh
constexpr uint32_t NUMBER_MODE_SLIDER = 2;
constexpr uint32_t FORMAT_PURPOSE_DEFAULT = 0, FORMAT_PURPOSE_ANNOUNCEMENT = 1;
constexpr uint32_t SERVICE_ARG_INT = 1, SERVICE_ARG_STRING = 3;  // api.proto ServiceArgType
constexpr size_t kClockTextMax = 64;  // Text «Clock text» max_length, UTF-8 characters
constexpr float kLedSpeedMin = 0.25f, kLedSpeedMax = 4.f;  // Number «LED speed»
// LightCommandRequest without a transition: ESPHome's default_transition_length. Longer ones are
// cut to a minute (frames run for the whole fade).
constexpr uint32_t kDefaultTransitionMs = 1000, kMaxTransitionMs = 60000;

constexpr size_t kMaxFrame = 1 << 20;            // reject absurd frames
constexpr size_t kAudioBacklogLimit = 256 << 10;  // end the voice run when the socket is this far behind
constexpr size_t kMaxBacklog = 8 << 20;           // give up on a client that stopped reading
constexpr size_t kReadPerWakeup = 64 << 10;
constexpr size_t kMaxClients = 4;  // HA plus e.g. the ESPHome dashboard log viewer / aioesphomeapi tools
constexpr size_t kMaxPending = 4;  // connections that have not sent a valid hello yet
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
struct StrState {
  bool has = false;
  std::string v;
};

float clamp01(float v) { return v < 0 ? 0 : (v > 1 ? 1 : v); }
size_t utf8_chars(const std::string &s) {
  size_t n = 0;
  for (unsigned char ch : s) n += (ch & 0xC0) != 0x80;
  return n;
}

// ESPHome Noise transport: frames are [0x01][len BE16][payload]. The first client frame is an
// empty "client hello", the server answers with a server hello, then one NNpsk0 round trip
// whose frames start with a status byte (0 = ok, 1 = reject + reason text). Data frames carry
// an encrypted [type BE16][len BE16][message].
constexpr const char *kNoiseProtocol = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";
constexpr size_t kNoiseMac = 16;
constexpr size_t kNoiseMaxFrame = 0xffff;
using Psk = std::array<uint8_t, 32>;

bool all_zero(const Psk &k) {
  return std::all_of(k.begin(), k.end(), [](uint8_t b) { return b == 0; });
}

// Standard base64 (padding optional); true only for exactly 32 decoded bytes.
bool decode_psk(const std::string &in, Psk *out) {
  size_t n = 0;
  uint32_t acc = 0;
  int bits = 0;
  for (char ch : in) {
    int v;
    if (ch >= 'A' && ch <= 'Z') v = ch - 'A';
    else if (ch >= 'a' && ch <= 'z') v = ch - 'a' + 26;
    else if (ch >= '0' && ch <= '9') v = ch - '0' + 52;
    else if (ch == '+') v = 62;
    else if (ch == '/') v = 63;
    else if (ch == '=') break;
    else return false;
    acc = (acc << 6) | uint32_t(v);
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      if (n == out->size()) return false;
      (*out)[n++] = uint8_t(acc >> bits);
    }
  }
  return n == out->size();
}

// wlan0 signal level in dBm from /proc/net/wireless ("wlan0: 0000   58.  -52.  -256 ...").
bool read_rssi(float *dbm) {
  FILE *f = fopen("/proc/net/wireless", "re");
  if (!f) return false;
  char line[256];
  bool ok = false;
  while (fgets(line, sizeof line, f)) {
    float link, level;
    if (sscanf(line, " wlan0: %*x %f %f", &link, &level) == 2) {
      *dbm = level;
      ok = true;
      break;
    }
  }
  fclose(f);
  return ok;
}

std::string trim(std::string s) {
  const char *ws = " \t\r\n";
  s.erase(0, s.find_first_not_of(ws));
  s.erase(s.find_last_not_of(ws) + 1);
  return s;
}

// Atomic replace: a power cut while HA rotates the key must leave either key, never a torn one.
bool write_file_atomic(const std::string &path, const std::string &data) {
  std::string tmp = path + ".tmp";
  int fd = open(tmp.c_str(), O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
  if (fd < 0) return false;
  bool ok = write(fd, data.data(), data.size()) == ssize_t(data.size()) && fsync(fd) == 0;
  ok = close(fd) == 0 && ok;
  if (!ok || rename(tmp.c_str(), path.c_str()) != 0) {
    unlink(tmp.c_str());
    return false;
  }
  return true;
}

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
    // Framing is chosen by the first byte the client sends: 0x00 plaintext, 0x01 Noise.
    enum class Proto : uint8_t { UNKNOWN, PLAIN, NOISE };
    enum class NoiseStep : uint8_t { CLIENT_HELLO, HANDSHAKE, DATA };

    int fd = -1;
    std::string peer;
    std::string rbuf;
    std::string wbuf;
    size_t woff = 0;
    bool want_out = false;
    bool closed = false;
    bool hello = false;
    bool states = false;
    bool voice = false;         // owns the voice pipeline
    uint64_t voice_wanted = 0;  // order of its SubscribeVoiceAssistant(true), 0 = not subscribed
    bool close_after_flush = false;  // also: ignore further input
    int64_t connected_at = 0;
    int64_t last_rx = 0;
    bool ping_outstanding = false;
    int64_t last_time_req = 0;
    uint32_t log_level = 0;  // SubscribeLogsRequest level (ESPHome LogLevel), 0 = none
    Proto proto = Proto::UNKNOWN;
    NoiseStep step = NoiseStep::CLIENT_HELLO;
    NoiseHandshakeState *hs = nullptr;
    NoiseCipherState *send_cs = nullptr;
    NoiseCipherState *recv_cs = nullptr;

    Client() = default;
    Client(const Client &) = delete;
    Client &operator=(const Client &) = delete;
    ~Client() {
      if (hs) noise_handshakestate_free(hs);
      if (send_cs) noise_cipherstate_free(send_cs);
      if (recv_cs) noise_cipherstate_free(recv_cs);
    }
  };
  using ClientPtr = std::shared_ptr<Client>;

  Loop &loop;
  DeviceIdentity id;
  ApiConfig cfg;
  ApiHooks &hooks;
  int listen_fd = -1;
  // All connections in accept order. A connection counts against kMaxClients only after a valid
  // HelloRequest (after the Noise handshake when encrypted), and only then may it evict an
  // established client, so port scanners and garbage cannot knock Home Assistant off.
  // Iterate over a copy: closing erases from here.
  std::vector<ClientPtr> clients;
  std::unique_ptr<MdnsResponder> mdns;
  uint64_t tick_timer = 0;
  bool audio_overflowed = false;  // mic audio stopped for this run (backlog)
  uint64_t voice_seq = 0;         // orders voice subscriptions (Client::voice_wanted)
  // Without a key Noise sessions run on the all-zeros key: ESPHome's way to let HA provision a
  // key without sending it in the clear (protects against sniffing only).
  Psk psk{};
  bool has_key = false;
  // Log lines are produced on any thread; the sink only posts a drain to the loop. The token
  // lets a drain posted just before destruction see that the server is gone.
  std::shared_ptr<bool> alive = std::make_shared<bool>(true);

  // Cached entity state (replayed on SubscribeStatesRequest).
  MediaState media_state = MediaState::IDLE;
  float media_volume = 0;
  bool media_muted = false;
  BoolState sw_mic_mute, sw_clock, sw_auto_update, sw_nearest_answers, sw_voice_indication, bs_hw_mute;
  UpdateState update;
  bool update_has = false;
  FloatState lux, clock_bri, clock_scroll, led_speed;
  StrState clock_text, clock_face, zigbee_mode, zigbee_status, music_viz, case_color, clock_music;
  LightState light;
  // Diagnostics, refreshed by diag_timer.
  FloatState rssi;
  uint32_t ip = 0;  // network byte order, as chosen by the mDNS responder
  const int64_t started_ms = Loop::now_ms();
  uint64_t diag_timer = 0;
  uint64_t key_timer = 0;

  Impl(Loop &l, DeviceIdentity i, ApiConfig c, ApiHooks &h) : loop(l), id(std::move(i)), cfg(std::move(c)), hooks(h) {}

  // Runs while the owner is being torn down: no hooks from here.
  ~Impl() {
    log_set_sink(4, nullptr);
    if (tick_timer) loop.cancel_timer(tick_timer);
    if (diag_timer) loop.cancel_timer(diag_timer);
    if (key_timer) loop.cancel_timer(key_timer);
    for (auto &c : clients) {
      loop.del_fd(c->fd);
      if (c->hello) {
        // Best effort, the loop no longer runs: with a DisconnectRequest HA treats the close
        // as expected and reconnects quietly once the daemon is back.
        append_frame(*c, DISCONNECT_REQ, pb::Writer());
        (void)!send(c->fd, c->wbuf.data() + c->woff, c->wbuf.size() - c->woff, MSG_NOSIGNAL | MSG_DONTWAIT);
        shutdown(c->fd, SHUT_WR);
        // Unread input at close() makes the kernel send RST, which may discard what was just sent.
        char buf[4096];
        while (recv(c->fd, buf, sizeof buf, MSG_DONTWAIT) > 0) {
        }
      }
      close(c->fd);
    }
    clients.clear();
    if (listen_fd >= 0) {
      loop.del_fd(listen_fd);
      close(listen_fd);
    }
  }

  bool ready() const {
    return std::any_of(clients.begin(), clients.end(), [](const ClientPtr &c) { return c->hello; });
  }
  bool states_on() const {
    return std::any_of(clients.begin(), clients.end(), [](const ClientPtr &c) { return c->hello && c->states; });
  }
  ClientPtr voice_client() const {
    for (const auto &c : clients)
      if (c->hello && c->voice) return c;
    return nullptr;
  }

  // The voice pipeline belongs to the client that subscribed most recently among those still
  // subscribed: HA reconnecting after a Wi-Fi drop takes it from its dead socket at once, and a
  // probe or second HA that subscribes and goes away hands it back to the one before it.
  // Hooks see false before true on every hand-over.
  void update_voice_owner() {
    ClientPtr want;
    for (const auto &c : clients)
      if (c->hello && c->voice_wanted && (!want || c->voice_wanted > want->voice_wanted)) want = c;
    ClientPtr cur = voice_client();
    if (cur == want) return;
    if (cur) {
      cur->voice = false;
      if (want) LOGI("api: voice assistant moves from %s to %s", cur->peer.c_str(), want->peer.c_str());
      hooks.on_voice_subscribed(false);
    }
    // The hook may have closed clients: only hand over to one that is still subscribed.
    if (want && !want->closed && want->voice_wanted && !voice_client()) {
      if (!cur) LOGI("api: voice assistant served by %s", want->peer.c_str());
      want->voice = true;
      hooks.on_voice_subscribed(true);
    }
  }
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
    diag_timer = loop.add_timer(kDiagPeriodMs, [this] { refresh_diagnostics(true); }, kDiagPeriodMs);
    LOGI("api: listening on :%u as '%s'", cfg.port, id.name.c_str());
    load_key();
    mdns.reset(new MdnsResponder(loop, id.name, "_esphomelib._tcp", cfg.port, txt_records()));
    if (!mdns->start()) LOGW("api: mDNS advertising unavailable");
    return true;
  }

  // "aabbccddeeff", as ESPHome puts it in mDNS TXT and the Noise server hello.
  std::string mac_plain() const {
    std::string m;
    for (char c : id.mac)
      if (c != ':') m.push_back(char(c >= 'A' && c <= 'F' ? c - 'A' + 'a' : c));
    return m;
  }

  std::vector<std::string> txt_records() const {
    std::vector<std::string> txt = {
        "friendly_name=" + id.friendly_name,
        std::string("version=") + kEsphomeVersion,
        "mac=" + mac_plain(),
        "platform=host",
        "board=" + id.board,
        "network=wifi",
        std::string("project_name=") + kProjectName,
        "project_version=" + id.project_version,
    };
    // HA reads these to ask for a key / offer to provision one before connecting.
    if (!cfg.key_path.empty())
      txt.push_back(std::string(has_key ? "api_encryption=" : "api_encryption_supported=") + kNoiseProtocol);
    return txt;
  }

  void load_key() {
    has_key = false;
    psk.fill(0);
    if (cfg.key_path.empty()) return;
    std::string s;
    if (FILE *f = fopen(cfg.key_path.c_str(), "re")) {
      char buf[256];
      size_t n = fread(buf, 1, sizeof buf, f);
      fclose(f);
      s = trim(std::string(buf, n));
    }
    if (s.empty()) {
      LOGI("api: no encryption key (%s), plaintext API", cfg.key_path.c_str());
      return;
    }
    Psk k;
    if (!decode_psk(s, &k)) {
      // Fail closed: someone meant to enable encryption, so do not fall back to plaintext. A
      // random key keeps every client out until the file is fixed (or removed).
      LOGE("api: %s is not base64 of 32 bytes; no client can connect until it is fixed", cfg.key_path.c_str());
      if (getrandom(psk.data(), psk.size(), 0) != ssize_t(psk.size())) psk.fill(0xa5);
      has_key = true;
      return;
    }
    if (all_zero(k)) return;
    psk = k;
    has_key = true;
    LOGI("api: Noise encryption enabled");
  }

  // NoiseEncryptionSetKeyRequest: empty key clears it. Like ESPHome, the reply goes out on the
  // old session, then every connection is closed so that only clients holding the new key
  // (or plaintext ones, after a clear) get back in.
  bool set_key(const std::string &b64) {
    if (cfg.key_path.empty()) return false;
    if (b64.empty()) {
      if (unlink(cfg.key_path.c_str()) != 0 && errno != ENOENT) {
        LOGW("api: cannot remove %s: %s", cfg.key_path.c_str(), strerror(errno));
        return false;
      }
      LOGI("api: encryption key cleared by client");
    } else {
      Psk k;
      if (!decode_psk(b64, &k) || all_zero(k)) {
        LOGW("api: rejected encryption key (must be base64 of 32 non-zero bytes)");
        return false;
      }
      if (has_key && k == psk) return true;
      if (!write_file_atomic(cfg.key_path, b64 + "\n")) {
        LOGW("api: cannot write %s: %s", cfg.key_path.c_str(), strerror(errno));
        return false;
      }
      LOGI("api: encryption key set by client");
    }
    if (key_timer) loop.cancel_timer(key_timer);
    key_timer = loop.add_timer(100, [this] {
      key_timer = 0;
      load_key();
      if (mdns) mdns->set_txt("_esphomelib._tcp", txt_records());
      LOGW("api: encryption key changed, disconnecting all clients");
      auto snapshot = clients;
      for (const auto &c : snapshot) {
        if (c->hello) {
          c->close_after_flush = true;
          send_msg(c, DISCONNECT_REQ, pb::Writer());
        } else {
          close_client(c, "encryption key changed");
        }
      }
    });
    return true;
  }

  // Wi-Fi drops leave half-open connections that never see a FIN. Keepalive probes detect an
  // idle dead peer in ~35 s and TCP_USER_TIMEOUT a stalled one (e.g. during mic streaming) in
  // 30 s, well before the API-level ping timeout.
  static void configure_socket(int fd) {
    int one = 1, idle = 20, intvl = 5, cnt = 3;
    unsigned user_timeout_ms = 30000;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, &one, sizeof one);
    setsockopt(fd, IPPROTO_TCP, TCP_KEEPIDLE, &idle, sizeof idle);
    setsockopt(fd, IPPROTO_TCP, TCP_KEEPINTVL, &intvl, sizeof intvl);
    setsockopt(fd, IPPROTO_TCP, TCP_KEEPCNT, &cnt, sizeof cnt);
    setsockopt(fd, IPPROTO_TCP, TCP_USER_TIMEOUT, &user_timeout_ms, sizeof user_timeout_ms);
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
      size_t unauthenticated =
          size_t(std::count_if(clients.begin(), clients.end(), [](const ClientPtr &c) { return !c->hello; }));
      if (unauthenticated >= kMaxPending) {
        auto oldest = *std::find_if(clients.begin(), clients.end(), [](const ClientPtr &c) { return !c->hello; });
        close_client(oldest, "superseded by newer connections");
      }
      configure_socket(fd);
      char ip[INET_ADDRSTRLEN];
      inet_ntop(AF_INET, &sa.sin_addr, ip, sizeof ip);
      auto c = std::make_shared<Client>();
      c->fd = fd;
      c->peer = std::string(ip) + ":" + std::to_string(ntohs(sa.sin_port));
      c->connected_at = c->last_rx = Loop::now_ms();
      clients.push_back(c);
      LOGI("api: connection from %s", c->peer.c_str());
      loop.add_fd(fd, EPOLLIN | EPOLLRDHUP, [this, c](uint32_t ev) { on_client_event(c, ev); });
    }
  }

  void close_client(ClientPtr c, const char *why) {
    if (c->closed) return;
    c->closed = true;
    loop.del_fd(c->fd);
    close(c->fd);
    c->fd = -1;
    clients.erase(std::remove(clients.begin(), clients.end(), c), clients.end());
    LOGI("api: %s %s (%s)", c->peer.c_str(), c->hello ? "disconnected" : "dropped", why);
    if (c->log_level) update_log_sink();
    if (!c->hello) return;
    if (c->voice) {
      c->voice = false;
      hooks.on_voice_subscribed(false);
    }
    update_voice_owner();
    if (!ready()) hooks.on_client_disconnected();
  }

  // Our levels 0..3 (D, I, W, E) <-> ESPHome LogLevel (DEBUG 5, INFO 3, WARN 2, ERROR 1).
  static uint32_t esphome_level(int level) {
    static const uint32_t map[] = {5, 3, 2, 1};
    return map[level & 3];
  }

  void update_log_sink() {
    uint32_t want = 0;
    for (const auto &c : clients)
      if (c->hello) want = std::max(want, c->log_level);
    if (!want) return log_set_sink(4, nullptr);
    int min = want >= 5 ? 0 : want >= 3 ? 1 : want == 2 ? 2 : 3;
    std::weak_ptr<bool> token = alive;
    log_set_sink(min, [this, token] {
      loop.post([this, token] {
        if (token.lock()) drain_logs();
      });
    });
  }

  void drain_logs() {
    for (const LogLine &l : log_take()) {
      uint32_t lvl = esphome_level(l.level);
      pb::Writer w;
      w.u32(1, lvl, true);
      w.str(3, l.text, true);
      auto snapshot = clients;
      for (const auto &c : snapshot)
        if (c->hello && c->log_level >= lvl) send_msg(c, SUBSCRIBE_LOGS_RESP, w);
    }
  }

  // A client completing hello at the cap replaces an established client: preferably one without
  // interest in the voice pipeline, never its owner, otherwise the one heard from least recently
  // (a reconnecting HA whose old socket has not timed out yet gets in, and the old one goes).
  void make_room() {
    ClientPtr victim;
    size_t established = 0;
    auto rank = [](const ClientPtr &c) { return c->voice ? 2 : c->voice_wanted ? 1 : 0; };
    for (const auto &c : clients) {
      if (!c->hello) continue;
      established++;
      if (!victim || rank(c) < rank(victim) || (rank(c) == rank(victim) && c->last_rx < victim->last_rx))
        victim = c;
    }
    if (established >= kMaxClients) close_client(victim, "replaced by a new connection");
  }

  void on_client_event(const ClientPtr &c, uint32_t ev) {
    if (c->closed) return;
    if (ev & EPOLLOUT) {
      flush(c);
      if (c->closed) return;
    }
    if (ev & (EPOLLIN | EPOLLRDHUP | EPOLLHUP | EPOLLERR)) process(c, read_into(c));
  }

  // Reads up to kReadPerWakeup bytes into c->rbuf; returns true on EOF/error. epoll is
  // level-triggered, so the rest is read on the next wakeup and a flooding peer cannot starve
  // the loop (capture hand-off, timers) or grow rbuf without bound before it is parsed.
  bool read_into(const ClientPtr &c) {
    char buf[16384];
    bool eof = false;
    size_t got = 0;
    while (got < kReadPerWakeup) {
      ssize_t n = recv(c->fd, buf, sizeof buf, 0);
      if (n > 0) {
        c->rbuf.append(buf, size_t(n));
        got += size_t(n);
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
    if (got) {
      c->last_rx = Loop::now_ms();
      c->ping_outstanding = false;
    }
    return eof;
  }

  void process(const ClientPtr &c, bool eof) {
    auto *p = reinterpret_cast<uint8_t *>(c->rbuf.data());
    const size_t size = c->rbuf.size();
    size_t pos = 0;
    while (!c->closed && !c->close_after_flush && pos < size) {
      if (c->proto == Client::Proto::UNKNOWN) {
        if (p[pos] == 0x01) {
          c->proto = Client::Proto::NOISE;
        } else if (p[pos] == 0x00 && !has_key) {
          c->proto = Client::Proto::PLAIN;
        } else if (p[pos] == 0x00) {
          noise_reject(c, "Bad indicator byte");  // aioesphomeapi: "requires encryption"
          break;
        } else {
          close_client(c, "bad frame preamble");
          return;
        }
      }
      size_t used = c->proto == Client::Proto::NOISE ? noise_frame(c, p + pos, size - pos)
                                                      : plain_frame(c, p + pos, size - pos);
      if (!used) break;  // incomplete, or the client was closed/rejected
      pos += used;
    }
    if (c->closed) return;
    if (c->close_after_flush)
      c->rbuf.clear();
    else
      c->rbuf.erase(0, pos);
    if (eof) close_client(c, "connection closed by peer");
  }

  // Each *_frame returns the bytes consumed, 0 if the frame is incomplete or the client is gone.
  size_t plain_frame(const ClientPtr &c, const uint8_t *p, size_t n) {
    if (p[0] != 0x00) {
      close_client(c, "bad frame preamble");
      return 0;
    }
    size_t q = 1;
    uint64_t len, type;
    if (!pb::get_varint(p, n, &q, &len) || !pb::get_varint(p, n, &q, &type)) {
      if (n > 21) close_client(c, "malformed frame header");
      return 0;
    }
    if (len > kMaxFrame) {
      close_client(c, "frame too large");
      return 0;
    }
    if (n - q < len) return 0;
    handle(c, uint32_t(type), p + q, size_t(len));
    return q + size_t(len);
  }

  size_t noise_frame(const ClientPtr &c, uint8_t *p, size_t n) {
    if (p[0] != 0x01) {
      noise_reject(c, "Bad indicator byte");
      return 0;
    }
    if (n < 3) return 0;
    size_t len = size_t(p[1]) << 8 | p[2];
    if (c->step != Client::NoiseStep::DATA && len > 128) {
      noise_reject(c, "Bad handshake packet len");
      return 0;
    }
    if (n - 3 < len) return 0;
    uint8_t *body = p + 3;
    switch (c->step) {
      case Client::NoiseStep::CLIENT_HELLO: noise_start(c, body, len); break;
      case Client::NoiseStep::HANDSHAKE: noise_handshake(c, body, len); break;
      case Client::NoiseStep::DATA: noise_data(c, body, len); break;
    }
    return 3 + len;
  }

  // Client hello (contents reserved, but bound into the handshake via the prologue): answer
  // with the server hello and prepare the responder side of NNpsk0.
  void noise_start(const ClientPtr &c, const uint8_t *body, size_t len) {
    std::string prologue = "NoiseAPIInit";
    prologue.push_back(char(len >> 8));
    prologue.push_back(char(len & 0xff));
    prologue.append(reinterpret_cast<const char *>(body), len);
    // Chosen protocol 0x01, then node name and MAC: aioesphomeapi checks both against what HA
    // expects before running the handshake.
    std::string hello(1, '\x01');
    hello += id.name;
    hello.push_back('\0');
    hello += mac_plain();
    hello.push_back('\0');
    append_noise_raw(*c, hello);

    NoiseProtocolId nid{};
    nid.prefix_id = NOISE_PREFIX_STANDARD;
    nid.pattern_id = NOISE_PATTERN_NN;
    nid.modifier_ids[0] = NOISE_MODIFIER_PSK0;
    nid.dh_id = NOISE_DH_CURVE25519;
    nid.cipher_id = NOISE_CIPHER_CHACHAPOLY;
    nid.hash_id = NOISE_HASH_SHA256;
    int err = noise_handshakestate_new_by_id(&c->hs, &nid, NOISE_ROLE_RESPONDER);
    if (!err) err = noise_handshakestate_set_pre_shared_key(c->hs, psk.data(), psk.size());
    if (!err) err = noise_handshakestate_set_prologue(c->hs, prologue.data(), prologue.size());
    if (!err) err = noise_handshakestate_start(c->hs);
    if (err) {
      LOGE("api: noise setup failed (%d)", err);
      close_client(c, "noise setup failed");
      return;
    }
    c->step = Client::NoiseStep::HANDSHAKE;
    flush(c);
  }

  void noise_handshake(const ClientPtr &c, uint8_t *body, size_t len) {
    if (noise_handshakestate_get_action(c->hs) != NOISE_ACTION_READ_MESSAGE) return noise_reject(c, "Handshake error");
    if (!len) return noise_reject(c, "Empty handshake message");
    if (body[0] != 0x00) return noise_reject(c, "Bad handshake error byte");
    NoiseBuffer in;
    noise_buffer_set_input(in, body + 1, len - 1);
    int err = noise_handshakestate_read_message(c->hs, &in, nullptr);
    if (err) {
      LOGW("api: %s noise handshake failed: %s", c->peer.c_str(),
           err == NOISE_ERROR_MAC_FAILURE ? "wrong encryption key" : "protocol error");
      // The exact "Handshake MAC failure" text makes clients report a wrong key.
      return noise_reject(c, err == NOISE_ERROR_MAC_FAILURE ? "Handshake MAC failure" : "Handshake error");
    }
    if (noise_handshakestate_get_action(c->hs) != NOISE_ACTION_WRITE_MESSAGE) return noise_reject(c, "Handshake error");
    uint8_t out[128];
    out[0] = 0x00;
    NoiseBuffer ob;
    noise_buffer_set_output(ob, out + 1, sizeof out - 1);
    err = noise_handshakestate_write_message(c->hs, &ob, nullptr);
    if (err || noise_handshakestate_get_action(c->hs) != NOISE_ACTION_SPLIT) return noise_reject(c, "Handshake error");
    append_noise_raw(*c, std::string(reinterpret_cast<const char *>(out), ob.size + 1));
    err = noise_handshakestate_split(c->hs, &c->send_cs, &c->recv_cs);
    noise_handshakestate_free(c->hs);
    c->hs = nullptr;
    if (err) {
      close_client(c, "noise split failed");
      return;
    }
    c->step = Client::NoiseStep::DATA;
    LOGI("api: %s encrypted session (%s)", c->peer.c_str(), has_key ? "device key" : "provisioning key");
    flush(c);
  }

  void noise_data(const ClientPtr &c, uint8_t *body, size_t len) {
    NoiseBuffer mb;
    noise_buffer_set_inout(mb, body, len, len);
    if (noise_cipherstate_decrypt(c->recv_cs, &mb) != NOISE_ERROR_NONE) {
      close_client(c, "noise decrypt failed");
      return;
    }
    if (mb.size < 4) {
      close_client(c, "short noise message");
      return;
    }
    uint32_t type = uint32_t(body[0]) << 8 | body[1];
    size_t dlen = size_t(body[2]) << 8 | body[3];
    if (dlen > mb.size - 4) {
      close_client(c, "bad noise message length");
      return;
    }
    handle(c, type, body + 4, dlen);
  }

  // Explicit handshake rejection (status byte 0x01 + reason), then close.
  void noise_reject(const ClientPtr &c, const char *reason) {
    LOGI("api: %s rejected: %s", c->peer.c_str(), reason);
    append_noise_raw(*c, std::string("\x01") + reason);
    c->close_after_flush = true;
    flush(c);
  }

  static void compact(Client &c) {
    if (c.woff && c.woff == c.wbuf.size()) {
      c.wbuf.clear();
      c.woff = 0;
    }
  }

  static void append_noise_raw(Client &c, const std::string &payload) {
    compact(c);
    c.wbuf.push_back(0x01);
    c.wbuf.push_back(char(payload.size() >> 8));
    c.wbuf.push_back(char(payload.size() & 0xff));
    c.wbuf += payload;
  }

  static void append_frame(Client &c, uint32_t type, const pb::Writer &w) {
    compact(c);
    if (c.proto != Client::Proto::NOISE) {
      c.wbuf.push_back(0);
      pb::put_varint(c.wbuf, w.size());
      pb::put_varint(c.wbuf, type);
      c.wbuf += w.data();
      return;
    }
    if (c.step != Client::NoiseStep::DATA) return;
    size_t body = 4 + w.size();
    if (body + kNoiseMac > kNoiseMaxFrame) {
      LOGE("api: message %u too large for a Noise frame (%zu bytes)", type, w.size());
      return;
    }
    size_t start = c.wbuf.size();
    c.wbuf.resize(start + 3 + body + kNoiseMac);
    auto *b = reinterpret_cast<uint8_t *>(c.wbuf.data()) + start;
    b[3] = uint8_t(type >> 8);
    b[4] = uint8_t(type);
    b[5] = uint8_t(w.size() >> 8);
    b[6] = uint8_t(w.size());
    memcpy(b + 7, w.data().data(), w.size());
    NoiseBuffer mb;
    noise_buffer_set_inout(mb, b + 3, body, body + kNoiseMac);
    if (noise_cipherstate_encrypt(c.send_cs, &mb) != NOISE_ERROR_NONE) {
      LOGE("api: noise encrypt failed");
      c.wbuf.resize(start);
      return;
    }
    b[0] = 0x01;
    b[1] = uint8_t(mb.size >> 8);
    b[2] = uint8_t(mb.size);
    c.wbuf.resize(start + 3 + mb.size);
  }

  void send_msg(const ClientPtr &c, uint32_t type, const pb::Writer &w) {
    if (c->closed) return;
    append_frame(*c, type, w);
    flush(c);
  }

  // State updates go to every client that subscribed to states.
  void broadcast(uint32_t type, const pb::Writer &w) {
    auto snapshot = clients;
    for (const auto &c : snapshot)
      if (c->hello && c->states) send_msg(c, type, w);
  }

  void flush(const ClientPtr &cp) {
    Client &c = *cp;
    if (c.closed) return;
    while (c.woff < c.wbuf.size()) {
      ssize_t n = send(c.fd, c.wbuf.data() + c.woff, c.wbuf.size() - c.woff, MSG_NOSIGNAL);
      if (n > 0) {
        c.woff += size_t(n);
        continue;
      }
      if (n < 0 && errno == EINTR) continue;
      if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
        if (c.wbuf.size() - c.woff > kMaxBacklog) {
          close_client(cp, "client not reading");
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
      close_client(cp, "write error");
      return;
    }
    c.wbuf.clear();
    c.woff = 0;
    if (c.want_out) {
      c.want_out = false;
      loop.mod_fd(c.fd, EPOLLIN | EPOLLRDHUP);
    }
    if (c.close_after_flush) close_client(cp, "disconnect requested");
  }

  static size_t backlog(const Client &c) { return c.wbuf.size() - c.woff; }

  void tick() {
    int64_t now = Loop::now_ms();
    auto snapshot = clients;
    for (const auto &c : snapshot) {
      if (c->closed) continue;
      if (!c->hello) {
        if (now - c->connected_at > kHelloTimeoutMs) close_client(c, "no hello");
        continue;
      }
      int64_t idle = now - c->last_rx;
      if (idle > kIdleDropMs) {
        close_client(c, "keepalive timeout");
        continue;
      }
      if (idle > kIdlePingMs && !c->ping_outstanding) {
        c->ping_outstanding = true;
        send_msg(c, PING_REQ, pb::Writer());
      }
      if (!c->closed && now - c->last_time_req >= kTimeResyncMs) request_time(c);
    }
  }

  void request_time(const ClientPtr &c) {
    c->last_time_req = Loop::now_ms();
    send_msg(c, GET_TIME_REQ, pb::Writer());
  }

  // ---------------------------------------------------------------- dispatch

  void handle(const ClientPtr &c, uint32_t type, const uint8_t *p, size_t n) {
    pb::Reader r(p, n);
    pb::Field f;
    if (!c->hello && type != HELLO_REQ && type != DISCONNECT_REQ && type != PING_REQ) {
      LOGW("api: message %u before hello", type);
      close_client(c, "protocol violation");
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
        w.str(3, std::string("pulsar ") + id.project_version, true);
        w.str(4, id.name, true);
        if (!c->hello) make_room();
        send_msg(c, HELLO_RESP, w);
        if (c->closed) return;
        LOGI("api: hello from '%s' (api %u.%u) at %s", info.c_str(), major, minor, c->peer.c_str());
        if (!c->hello) {
          c->hello = true;
          hooks.on_client_connected(info);
          if (!c->closed) request_time(c);
        }
        break;
      }
      case DISCONNECT_REQ:
        c->close_after_flush = true;
        send_msg(c, DISCONNECT_RESP, pb::Writer());
        break;
      case DISCONNECT_RESP:
        close_client(c, "disconnect acknowledged");
        break;
      case PING_REQ:
        send_msg(c, PING_RESP, pb::Writer());
        break;
      case PING_RESP:
        break;
      case DEVICE_INFO_REQ:
        send_device_info(c);
        break;
      case DEVICE_CAPS_REQ: {
        pb::Writer va, w;
        va.u32(1, kVoiceFeatures);
        w.msg(2, va);
        send_msg(c, DEVICE_CAPS_RESP, w);
        break;
      }
      case LIST_ENTITIES_REQ:
        list_entities(c);
        break;
      case SUBSCRIBE_STATES_REQ:
        c->states = true;
        send_all_states(c);
        break;
      case SUBSCRIBE_LOGS_REQ: {
        uint32_t level = 0;
        while (r.next(f))
          if (f.num == 1) level = f.u32();
        LOGI("api: %s log subscription, level %u", c->peer.c_str(), level);
        c->log_level = level;
        update_log_sink();
        break;
      }
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
      case BUTTON_CMD: {
        uint32_t key = 0;
        while (r.next(f))
          if (f.num == 1) key = f.u32();
        if (key == K_BUTTON_ZIGBEE_JOIN || key == K_BUTTON_ZIGBEE_LEAVE) {
          if (cfg.zigbee_modes.empty()) break;
          LOGI("api: Zigbee button %u pressed by %s", unsigned(key), c->peer.c_str());
          hooks.on_button(key == K_BUTTON_ZIGBEE_JOIN ? ButtonId::ZIGBEE_JOIN : ButtonId::ZIGBEE_LEAVE);
          break;
        }
        if (key != K_BUTTON_RESTART) break;
        LOGW("api: restart requested by %s", c->peer.c_str());
        hooks.on_restart();
        return;  // the server may be going away
      }
      case SWITCH_CMD:
        on_switch_cmd(r);
        break;
      case UPDATE_CMD: {
        uint32_t key = 0, cmd = 0;
        while (r.next(f)) {
          if (f.num == 1) key = f.u32();
          if (f.num == 2) cmd = f.u32();
        }
        if (key == K_UPDATE && cmd <= uint32_t(UpdateCommand::CHECK)) hooks.on_update(UpdateCommand(cmd));
        break;
      }
      case NUMBER_CMD:
        on_number_cmd(r);
        break;
      case TEXT_CMD:
        on_text_cmd(r);
        break;
      case SELECT_CMD:
        on_select_cmd(r);
        break;
      case EXECUTE_SERVICE_REQ:
        on_execute_service(r);
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
        LOGI("api: %s voice assistant %s (flags=%u)", c->peer.c_str(), sub ? "subscribed" : "unsubscribed", flags);
        c->voice_wanted = sub ? ++voice_seq : 0;
        update_voice_owner();
        break;
      }
      case VA_RESPONSE: {
        if (!c->voice) break;  // answers to our requests only come from the pipeline owner
        uint32_t port = 0;
        bool error = false;
        while (r.next(f)) {
          if (f.num == 1) port = f.u32();
          if (f.num == 2) error = f.boolean();
        }
        if (error) {
          LOGW("api: HA failed to start the voice pipeline");
          hooks.on_voice_start_failed();
        } else {
          LOGI("api: voice pipeline started (%s audio)", port ? "UDP" : "API");
        }
        break;
      }
      case VA_EVENT_RESP: {
        if (!c->voice) break;
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
        send_va_config(c);
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
        // The spotter always runs one model: an empty/unknown selection leaves the current word
        // active, and the controller reports a switch back through set_active_wake_words().
        if (ids.empty()) {
          LOGW("api: no known wake word in the new configuration, keeping the current one");
          break;
        }
        hooks.on_active_wake_words(ids);
        break;
      }
      case NOISE_SET_KEY_REQ: {
        std::string key;
        while (r.next(f))
          if (f.num == 1) key = f.str();
        pb::Writer w;
        w.boolean(1, set_key(trim(key)));
        send_msg(c, NOISE_SET_KEY_RESP, w);
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

  void send_device_info(const ClientPtr &c) {
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
    w.boolean(19, !cfg.key_path.empty());              // api_encryption_supported
    w.boolean(26, !cfg.key_path.empty() && !has_key);  // api_encryption_provisionable
    send_msg(c, DEVICE_INFO_RESP, w);
  }

  static void entity_head(pb::Writer &w, const char *object_id, uint32_t key, const char *name) {
    w.str(1, object_id, true);
    w.fixed32(2, key, true);
    w.str(3, name, true);
  }

  void list_entities(const ClientPtr &c) {
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
      send_msg(c, LIST_MEDIA_PLAYER, w);
    }
    auto sw = [&](const char *oid, uint32_t key, const char *name, const char *icon) {
      pb::Writer w;
      entity_head(w, oid, key, name);
      w.str(5, icon);
      send_msg(c, LIST_SWITCH, w);
    };
    sw("mute_microphones", K_SW_MIC_MUTE, "Mute microphones", "mdi:microphone-off");
    sw("clock_display", K_SW_CLOCK, "Clock display", "mdi:clock-digital");
    {
      pb::Writer w;
      entity_head(w, "auto_update", K_SW_AUTO_UPDATE, "Auto update");
      w.str(5, "mdi:update");
      w.u32(8, ENTITY_CATEGORY_CONFIG);
      send_msg(c, LIST_SWITCH, w);
    }
    {
      pb::Writer w;
      entity_head(w, "nearest_station_answers", K_SW_NEAREST_ANSWERS, "Nearest station answers");
      w.str(5, "mdi:speaker-multiple");
      w.u32(8, ENTITY_CATEGORY_CONFIG);
      send_msg(c, LIST_SWITCH, w);
    }
    {
      pb::Writer w;
      entity_head(w, "voice_indication", K_SW_VOICE_INDICATION, "Voice indication");
      w.str(5, "mdi:led-on");
      w.u32(8, ENTITY_CATEGORY_CONFIG);
      send_msg(c, LIST_SWITCH, w);
    }
    {
      pb::Writer w;
      entity_head(w, "led_speed", K_NUMBER_LED_SPEED, "LED speed");
      w.str(5, "mdi:speedometer");
      w.flt(6, kLedSpeedMin);
      w.flt(7, kLedSpeedMax);
      w.flt(8, 0.05f);
      w.u32(10, ENTITY_CATEGORY_CONFIG);
      w.str(11, "×");
      w.u32(12, NUMBER_MODE_SLIDER);
      send_msg(c, LIST_NUMBER, w);
    }
    auto select_entity = [&](const char *oid, uint32_t key, const char *name, const char *icon,
                             const std::vector<std::string> &options) {
      pb::Writer w;
      entity_head(w, oid, key, name);
      w.str(5, icon);
      for (const auto &o : options) w.str(6, o, true);
      w.u32(8, ENTITY_CATEGORY_CONFIG);
      send_msg(c, LIST_SELECT, w);
    };
    select_entity("music_visualization", K_SELECT_MUSIC_VIZ, "Music visualization", "mdi:equalizer",
                  cfg.music_styles);
    select_entity("case_color", K_SELECT_CASE_COLOR, "Case color", "mdi:palette", cfg.case_colors);
    select_entity("clock_music", K_SELECT_CLOCK_MUSIC, "Clock music", "mdi:equalizer-outline",
                  cfg.clock_music_styles);
    {
      pb::Writer w;
      entity_head(w, "firmware", K_UPDATE, "Firmware");
      w.u32(7, ENTITY_CATEGORY_CONFIG);
      w.str(8, "firmware");
      send_msg(c, LIST_UPDATE, w);
    }
    {
      pb::Writer w;
      entity_head(w, "hardware_mic_mute", K_BS_HW_MIC_MUTE, "Hardware mic mute");
      w.str(8, "mdi:microphone-off");
      w.u32(9, ENTITY_CATEGORY_DIAGNOSTIC);
      send_msg(c, LIST_BINARY_SENSOR, w);
    }
    {
      pb::Writer w;
      entity_head(w, "illuminance", K_SENSOR_LUX, "Illuminance");
      w.str(6, "lx");
      w.i32(7, 0);
      w.str(9, "illuminance");
      w.u32(10, STATE_CLASS_MEASUREMENT);
      send_msg(c, LIST_SENSOR, w);
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
      send_msg(c, LIST_NUMBER, w);
    }
    {
      pb::Writer w;
      entity_head(w, "ring", K_LIGHT_RING, "Ring");
      w.str(11, "None", true);
      for (const auto &e : cfg.light_effects) w.str(11, e, true);
      w.packed_u32(12, {COLOR_MODE_RGB});
      w.str(14, "mdi:led-strip-variant");
      send_msg(c, LIST_LIGHT, w);
    }
    {
      pb::Writer w;
      entity_head(w, "button", K_EVENT_BUTTON, "Button");
      w.str(5, "mdi:gesture-tap-button");
      w.str(8, "button");
      for (const char *t : {"action", "action_long", "volume_up", "volume_down"}) w.str(9, t, true);
      send_msg(c, LIST_EVENT, w);
    }
    {
      pb::Writer w;
      entity_head(w, "restart", K_BUTTON_RESTART, "Restart");
      w.u32(7, ENTITY_CATEGORY_CONFIG);
      w.str(8, "restart");
      send_msg(c, LIST_BUTTON, w);
    }
    {
      pb::Writer w;
      entity_head(w, "wifi_signal", K_SENSOR_RSSI, "Wi-Fi signal");
      w.str(6, "dBm");
      w.str(9, "signal_strength");
      w.u32(10, STATE_CLASS_MEASUREMENT);
      w.u32(13, ENTITY_CATEGORY_DIAGNOSTIC);
      send_msg(c, LIST_SENSOR, w);
    }
    {
      pb::Writer w;
      entity_head(w, "uptime", K_SENSOR_UPTIME, "Uptime");
      w.str(6, "s");
      w.str(9, "duration");
      w.u32(10, STATE_CLASS_TOTAL_INCREASING);
      w.u32(13, ENTITY_CATEGORY_DIAGNOSTIC);
      send_msg(c, LIST_SENSOR, w);
    }
    auto text = [&](const char *oid, uint32_t key, const char *name, const char *icon) {
      pb::Writer w;
      entity_head(w, oid, key, name);
      w.str(5, icon);
      w.u32(7, ENTITY_CATEGORY_DIAGNOSTIC);
      send_msg(c, LIST_TEXT_SENSOR, w);
    };
    text("ip_address", K_TEXT_IP, "IP address", "mdi:ip-network");
    text("version", K_TEXT_VERSION, "Version", "mdi:new-box");
    {
      pb::Writer w;
      entity_head(w, "clock_text_speed", K_NUMBER_CLOCK_SCROLL, "Clock text speed");
      w.str(5, "mdi:speedometer");
      w.flt(6, 100);
      w.flt(7, 2000);
      w.flt(8, 50);
      w.u32(10, ENTITY_CATEGORY_CONFIG);
      w.str(11, "ms");
      w.u32(12, NUMBER_MODE_SLIDER);
      send_msg(c, LIST_NUMBER, w);
    }
    {
      pb::Writer w;
      entity_head(w, "clock_text", K_TEXT_CLOCK, "Clock text");
      w.str(5, "mdi:form-textbox");
      w.u32(9, kClockTextMax);  // max_length
      send_msg(c, LIST_TEXT, w);
    }
    {
      pb::Writer w;
      entity_head(w, "clock_face", K_SELECT_CLOCK_FACE, "Clock face");
      w.str(5, "mdi:emoticon-outline");
      for (const auto &o : cfg.clock_faces) w.str(6, o, true);
      send_msg(c, LIST_SELECT, w);
    }
    if (!cfg.zigbee_modes.empty()) {
      select_entity("zigbee_role", K_SELECT_ZIGBEE_MODE, "Zigbee role", "mdi:zigbee", cfg.zigbee_modes);
      auto button = [&](const char *oid, uint32_t key, const char *name, const char *icon) {
        pb::Writer w;
        entity_head(w, oid, key, name);
        w.str(5, icon);
        w.u32(7, ENTITY_CATEGORY_CONFIG);
        send_msg(c, LIST_BUTTON, w);
      };
      button("zigbee_join", K_BUTTON_ZIGBEE_JOIN, "Zigbee join network", "mdi:link-variant");
      button("zigbee_leave", K_BUTTON_ZIGBEE_LEAVE, "Zigbee leave network", "mdi:link-variant-off");
      text("zigbee", K_TEXT_ZIGBEE, "Zigbee", "mdi:zigbee");
    }
    // User-defined actions (HA registers esphome.<device>_<name>; every argument is required).
    auto action = [&](const char *name, uint32_t key, std::initializer_list<std::pair<const char *, uint32_t>> args) {
      pb::Writer w;
      w.str(1, name, true);
      w.fixed32(2, key, true);
      for (auto &[arg, type] : args) {
        pb::Writer a;
        a.str(1, arg, true);
        a.u32(2, type);
        w.msg(3, a);
      }
      send_msg(c, LIST_SERVICES, w);
    };
    action("clock_show_text", K_ACTION_CLOCK_TEXT, {{"text", SERVICE_ARG_STRING}, {"seconds", SERVICE_ARG_INT}});
    action("clock_show_face", K_ACTION_CLOCK_FACE, {{"face", SERVICE_ARG_STRING}, {"seconds", SERVICE_ARG_INT}});
    action("led_show", K_ACTION_LED_SHOW,
           {{"effect", SERVICE_ARG_STRING}, {"color", SERVICE_ARG_STRING}, {"seconds", SERVICE_ARG_INT}});
    send_msg(c, LIST_DONE, pb::Writer());
  }

  // ---------------------------------------------------------------- states
  // `to` = one client (initial states after SubscribeStatesRequest) or null = every subscriber.

  void emit(const ClientPtr &to, uint32_t type, const pb::Writer &w) {
    if (to)
      send_msg(to, type, w);
    else
      broadcast(type, w);
  }

  void send_media(const ClientPtr &to = nullptr) {
    pb::Writer w;
    w.fixed32(1, K_MEDIA, true);
    w.u32(2, uint32_t(media_state));
    w.flt(3, media_volume);
    w.boolean(4, media_muted);
    emit(to, MEDIA_PLAYER_STATE, w);
  }
  void send_switch(uint32_t key, const BoolState &s, const ClientPtr &to = nullptr) {
    pb::Writer w;
    w.fixed32(1, key, true);
    w.boolean(2, s.v);
    w.boolean(4, !s.has);  // missing_state
    emit(to, SWITCH_STATE, w);
  }
  void send_binary(uint32_t key, const BoolState &s, const ClientPtr &to = nullptr) {
    pb::Writer w;
    w.fixed32(1, key, true);
    w.boolean(2, s.v);
    w.boolean(3, !s.has);
    emit(to, BINARY_SENSOR_STATE, w);
  }
  void send_float(uint32_t type, uint32_t key, const FloatState &s, const ClientPtr &to = nullptr) {
    pb::Writer w;
    w.fixed32(1, key, true);
    w.flt(2, s.v);
    w.boolean(3, !s.has);
    emit(to, type, w);
  }
  void send_light(const ClientPtr &to = nullptr) {
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
    emit(to, LIGHT_STATE, w);
  }

  void send_all_states(const ClientPtr &c) {
    send_media(c);
    send_switch(K_SW_MIC_MUTE, sw_mic_mute, c);
    send_switch(K_SW_CLOCK, sw_clock, c);
    send_switch(K_SW_AUTO_UPDATE, sw_auto_update, c);
    send_switch(K_SW_NEAREST_ANSWERS, sw_nearest_answers, c);
    send_switch(K_SW_VOICE_INDICATION, sw_voice_indication, c);
    send_update(c);
    send_binary(K_BS_HW_MIC_MUTE, bs_hw_mute, c);
    send_float(SENSOR_STATE, K_SENSOR_LUX, lux, c);
    send_float(NUMBER_STATE, K_NUMBER_CLOCK_BRI, clock_bri, c);
    send_light(c);
    refresh_diagnostics(false);
    send_float(SENSOR_STATE, K_SENSOR_RSSI, rssi, c);
    send_uptime(c);
    send_ip(c);
    send_str(TEXT_SENSOR_STATE, K_TEXT_VERSION, id.project_version, true, c);
    send_float(NUMBER_STATE, K_NUMBER_CLOCK_SCROLL, clock_scroll, c);
    send_str(TEXT_STATE, K_TEXT_CLOCK, clock_text.v, clock_text.has, c);
    send_str(SELECT_STATE, K_SELECT_CLOCK_FACE, clock_face.v, clock_face.has, c);
    send_float(NUMBER_STATE, K_NUMBER_LED_SPEED, led_speed, c);
    send_str(SELECT_STATE, K_SELECT_MUSIC_VIZ, music_viz.v, music_viz.has, c);
    send_str(SELECT_STATE, K_SELECT_CASE_COLOR, case_color.v, case_color.has, c);
    send_str(SELECT_STATE, K_SELECT_CLOCK_MUSIC, clock_music.v, clock_music.has, c);
    if (!cfg.zigbee_modes.empty()) {
      send_str(SELECT_STATE, K_SELECT_ZIGBEE_MODE, zigbee_mode.v, zigbee_mode.has, c);
      send_str(TEXT_SENSOR_STATE, K_TEXT_ZIGBEE, zigbee_status.v, zigbee_status.has, c);
    }
  }

  void send_update(const ClientPtr &to = nullptr) {
    pb::Writer w;
    w.fixed32(1, K_UPDATE, true);
    w.boolean(2, !update_has);  // missing_state
    w.boolean(3, update.in_progress);
    w.boolean(4, update.has_progress);
    w.flt(5, update.progress);
    w.str(6, update.current_version);
    w.str(7, update.latest_version);
    w.str(8, update.title);
    w.str(10, update.release_url);
    emit(to, UPDATE_STATE, w);
  }
  // TextSensorState, TextState and SelectState share the layout key / state / missing_state.
  void send_str(uint32_t type, uint32_t key, const std::string &s, bool has, const ClientPtr &to = nullptr) {
    pb::Writer w;
    w.fixed32(1, key, true);
    w.str(2, s);
    w.boolean(3, !has);
    emit(to, type, w);
  }
  void send_ip(const ClientPtr &to = nullptr) {
    char buf[INET_ADDRSTRLEN] = "";
    if (ip) inet_ntop(AF_INET, &ip, buf, sizeof buf);
    send_str(TEXT_SENSOR_STATE, K_TEXT_IP, buf, ip != 0, to);
  }
  void send_uptime(const ClientPtr &to = nullptr) {
    send_float(SENSOR_STATE, K_SENSOR_UPTIME, {true, float((Loop::now_ms() - started_ms) / 1000)}, to);
  }

  // RSSI and uptime go out every period, the IP only when it changes; publish=false only
  // refreshes the cache (initial states are sent by the caller).
  void refresh_diagnostics(bool publish) {
    float dbm;
    rssi.has = read_rssi(&dbm);
    if (rssi.has) rssi.v = dbm;
    uint32_t now_ip = mdns ? mdns->ipv4() : 0;
    bool ip_changed = now_ip != ip;
    ip = now_ip;
    if (!publish || !states_on()) return;
    send_float(SENSOR_STATE, K_SENSOR_RSSI, rssi);
    send_uptime();
    if (ip_changed) send_ip();
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
    else if (key == K_SW_AUTO_UPDATE)
      hooks.on_switch(SwitchId::AUTO_UPDATE, state);
    else if (key == K_SW_NEAREST_ANSWERS)
      hooks.on_switch(SwitchId::NEAREST_ANSWERS, state);
    else if (key == K_SW_VOICE_INDICATION)
      hooks.on_switch(SwitchId::VOICE_INDICATION, state);
  }

  void on_number_cmd(pb::Reader &r) {
    uint32_t key = 0;
    float v = 0;
    pb::Field f;
    while (r.next(f)) {
      if (f.num == 1) key = f.u32();
      if (f.num == 2) v = f.flt();
    }
    if (v != v) return;
    if (key == K_NUMBER_CLOCK_BRI)
      hooks.on_number(NumberId::CLOCK_BRIGHTNESS, std::min(100.0f, std::max(0.0f, v)));
    else if (key == K_NUMBER_CLOCK_SCROLL)
      hooks.on_number(NumberId::CLOCK_SCROLL, std::min(2000.0f, std::max(100.0f, v)));
    else if (key == K_NUMBER_LED_SPEED)
      hooks.on_number(NumberId::LED_SPEED, std::min(kLedSpeedMax, std::max(kLedSpeedMin, v)));
  }

  bool is_clock_face(const std::string &s) const {
    for (const auto &o : cfg.clock_faces)
      if (o == s) return true;
    return false;
  }

  void on_text_cmd(pb::Reader &r) {
    uint32_t key = 0;
    std::string s;
    pb::Field f;
    while (r.next(f)) {
      if (f.num == 1) key = f.u32();
      if (f.num == 2) s = f.str();
    }
    if (key != K_TEXT_CLOCK) return;
    if (utf8_chars(s) > kClockTextMax) {
      LOGW("api: clock text too long (%zu chars)", utf8_chars(s));
      return;
    }
    hooks.on_text(TextId::CLOCK_TEXT, s);
  }

  void on_select_cmd(pb::Reader &r) {
    uint32_t key = 0;
    std::string s;
    pb::Field f;
    while (r.next(f)) {
      if (f.num == 1) key = f.u32();
      if (f.num == 2) s = f.str();
    }
    if (key == K_SELECT_ZIGBEE_MODE) {
      for (const auto &o : cfg.zigbee_modes)
        if (o == s) return hooks.on_select(SelectId::ZIGBEE_MODE, s);
      LOGW("api: unknown Zigbee role '%s'", s.c_str());
      return;
    }
    if (key == K_SELECT_CLOCK_MUSIC) {
      for (const auto &o : cfg.clock_music_styles)
        if (o == s) return hooks.on_select(SelectId::CLOCK_MUSIC, s);
      LOGW("api: unknown clock music '%s'", s.c_str());
      return;
    }
    if (key == K_SELECT_MUSIC_VIZ || key == K_SELECT_CASE_COLOR) {
      const bool viz = key == K_SELECT_MUSIC_VIZ;
      for (const auto &o : viz ? cfg.music_styles : cfg.case_colors)
        if (o == s) return hooks.on_select(viz ? SelectId::MUSIC_VIZ : SelectId::CASE_COLOR, s);
      LOGW("api: unknown %s '%s'", viz ? "music visualization" : "case color", s.c_str());
      return;
    }
    if (key != K_SELECT_CLOCK_FACE) return;
    if (!is_clock_face(s)) {
      LOGW("api: unknown clock face '%s'", s.c_str());
      return;
    }
    hooks.on_select(SelectId::CLOCK_FACE, s);
  }

  void on_execute_service(pb::Reader &r) {
    struct Arg {
      std::string s;
      int32_t i = 0;
    };
    uint32_t key = 0;
    std::vector<Arg> args;
    pb::Field f;
    while (r.next(f)) {
      if (f.num == 1) key = f.u32();
      if (f.num != 2) continue;  // 3/4 call_id / return_response: actions declare no response
      Arg a;
      pb::Reader ar(f);
      pb::Field af;
      while (ar.next(af)) {
        if (af.num == 4) a.s = af.str();
        if (af.num == 5) a.i = int32_t(uint32_t(af.v >> 1) ^ -uint32_t(af.v & 1));  // sint32 zigzag
        if (af.num == 2) a.i = int32_t(uint32_t(af.v));                            // legacy_int
      }
      args.push_back(std::move(a));
    }
    const size_t want = key == K_ACTION_LED_SHOW ? 3 : 2;
    if ((key != K_ACTION_CLOCK_TEXT && key != K_ACTION_CLOCK_FACE && key != K_ACTION_LED_SHOW) ||
        args.size() != want) {
      LOGW("api: bad action call key=%u args=%zu", key, args.size());
      return;
    }
    if (key == K_ACTION_LED_SHOW) {
      const std::string &effect = args[0].s;
      if (!effect.empty() && effect != "None" &&
          std::find(cfg.light_effects.begin(), cfg.light_effects.end(), effect) == cfg.light_effects.end()) {
        LOGW("api: unknown LED effect '%s'", effect.c_str());
        return;
      }
      hooks.on_led_show(effect == "None" ? std::string() : effect, args[1].s, args[2].i);
      return;
    }
    if (key == K_ACTION_CLOCK_TEXT) {
      if (utf8_chars(args[0].s) > kClockTextMax) {
        LOGW("api: clock text too long (%zu chars)", utf8_chars(args[0].s));
        return;
      }
      hooks.on_action(ActionId::CLOCK_SHOW_TEXT, args[0].s, args[1].i);
    } else {
      if (!args[0].s.empty() && !is_clock_face(args[0].s)) {
        LOGW("api: unknown clock face '%s'", args[0].s.c_str());
        return;
      }
      hooks.on_action(ActionId::CLOCK_SHOW_FACE, args[0].s, args[1].i);
    }
  }

  void on_light_cmd(pb::Reader &r) {
    uint32_t key = 0;
    bool has_state = false, state = false, has_bri = false, has_cbri = false, has_rgb = false, has_effect = false,
         has_transition = false;
    uint32_t transition = 0;
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
        case 14: has_transition = f.boolean(); break;
        case 15: transition = f.u32(); break;
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
    hooks.on_light(st, has_transition ? std::min(transition, kMaxTransitionMs) : kDefaultTransitionMs);
    light = st;
    if (states_on()) send_light();
  }

  void send_va_config(const ClientPtr &c) {
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
    send_msg(c, VA_CONFIG_RESP, w);
  }
};

// ------------------------------------------------------------------ public API

ApiServer::ApiServer(Loop &loop, DeviceIdentity id, ApiConfig cfg, ApiHooks &hooks)
    : impl_(new Impl(loop, std::move(id), std::move(cfg), hooks)) {}

ApiServer::~ApiServer() = default;

bool ApiServer::start() { return impl_->start(); }

bool ApiServer::connected() const { return impl_->ready(); }

bool ApiServer::voice_subscribed() const { return impl_->voice_client() != nullptr; }
MdnsResponder *ApiServer::mdns() { return impl_->mdns.get(); }

void ApiServer::voice_request_start(const std::string &wake_word_phrase) {
  auto vc = impl_->voice_client();
  if (!vc) {
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
  impl_->audio_overflowed = false;
  impl_->send_msg(vc, VA_REQUEST, w);
}

void ApiServer::voice_request_stop() {
  auto vc = impl_->voice_client();
  if (!vc) return;
  pb::Writer w;
  w.boolean(1, false);
  impl_->send_msg(vc, VA_REQUEST, w);
}

void ApiServer::voice_send_audio(const int16_t *pcm, size_t samples) {
  auto vc = impl_->voice_client();
  if (!vc || !samples || impl_->audio_overflowed) return;
  // A hole in the utterance would make HA transcribe garbage: end the run instead.
  if (Impl::backlog(*vc) > kAudioBacklogLimit) {
    LOGW("api: network backlog, ending the voice pipeline");
    impl_->audio_overflowed = true;
    voice_request_stop();
    impl_->hooks.on_voice_audio_overflow();
    return;
  }
  pb::Writer w;
  w.bytes(1, pcm, samples * sizeof(int16_t));
  impl_->send_msg(vc, VA_AUDIO, w);
}

void ApiServer::voice_announce_finished(bool success) {
  auto vc = impl_->voice_client();
  if (!vc) return;
  pb::Writer w;
  w.boolean(1, success);
  impl_->send_msg(vc, VA_ANNOUNCE_FINISHED, w);
}

void ApiServer::set_active_wake_words(const std::vector<std::string> &ids) { impl_->cfg.active_wake_words = ids; }

void ApiServer::publish_media(MediaState st, float volume, bool muted) {
  impl_->media_state = st;
  impl_->media_volume = clamp01(volume);
  impl_->media_muted = muted;
  if (impl_->states_on()) impl_->send_media();
}

void ApiServer::publish_switch(SwitchId id, bool state) {
  BoolState *s = nullptr;
  uint32_t key = 0;
  switch (id) {
    case SwitchId::MIC_MUTE: s = &impl_->sw_mic_mute; key = K_SW_MIC_MUTE; break;
    case SwitchId::CLOCK: s = &impl_->sw_clock; key = K_SW_CLOCK; break;
    case SwitchId::AUTO_UPDATE: s = &impl_->sw_auto_update; key = K_SW_AUTO_UPDATE; break;
    case SwitchId::NEAREST_ANSWERS: s = &impl_->sw_nearest_answers; key = K_SW_NEAREST_ANSWERS; break;
    case SwitchId::VOICE_INDICATION: s = &impl_->sw_voice_indication; key = K_SW_VOICE_INDICATION; break;
  }
  if (!s) return;
  s->has = true;
  s->v = state;
  if (impl_->states_on()) impl_->send_switch(key, *s);
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

void ApiServer::publish_number(NumberId id, float value) {
  FloatState *s = nullptr;
  uint32_t key = 0;
  switch (id) {
    case NumberId::CLOCK_BRIGHTNESS: s = &impl_->clock_bri; key = K_NUMBER_CLOCK_BRI; break;
    case NumberId::CLOCK_SCROLL: s = &impl_->clock_scroll; key = K_NUMBER_CLOCK_SCROLL; break;
    case NumberId::LED_SPEED: s = &impl_->led_speed; key = K_NUMBER_LED_SPEED; break;
  }
  if (!s) return;
  s->has = true;
  s->v = value;
  if (impl_->states_on()) impl_->send_float(NUMBER_STATE, key, *s);
}

void ApiServer::publish_text(TextId, const std::string &state) {
  impl_->clock_text = {true, state};
  if (impl_->states_on()) impl_->send_str(TEXT_STATE, K_TEXT_CLOCK, state, true);
}

void ApiServer::publish_select(SelectId id, const std::string &state) {
  StrState *s = nullptr;
  uint32_t key = 0;
  switch (id) {
    case SelectId::CLOCK_FACE: s = &impl_->clock_face; key = K_SELECT_CLOCK_FACE; break;
    case SelectId::ZIGBEE_MODE: s = &impl_->zigbee_mode; key = K_SELECT_ZIGBEE_MODE; break;
    case SelectId::MUSIC_VIZ: s = &impl_->music_viz; key = K_SELECT_MUSIC_VIZ; break;
    case SelectId::CASE_COLOR: s = &impl_->case_color; key = K_SELECT_CASE_COLOR; break;
    case SelectId::CLOCK_MUSIC: s = &impl_->clock_music; key = K_SELECT_CLOCK_MUSIC; break;
  }
  if (!s) return;
  *s = {true, state};
  if (impl_->states_on()) impl_->send_str(SELECT_STATE, key, state, true);
}

void ApiServer::publish_text_sensor(TextSensorId, const std::string &state) {
  impl_->zigbee_status = {true, state};
  if (impl_->states_on() && !impl_->cfg.zigbee_modes.empty())
    impl_->send_str(TEXT_SENSOR_STATE, K_TEXT_ZIGBEE, state, true);
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
  impl_->broadcast(EVENT_RESP, w);
}

void ApiServer::publish_update(const UpdateState &st) {
  impl_->update = st;
  impl_->update_has = true;
  if (impl_->states_on()) impl_->send_update();
}

}  // namespace cs
