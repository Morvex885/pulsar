// See sendspin_shim.h. Policy (formats, pairing, visualizer layout, persistence, ALSA output)
// lives in the Rust service; this file only maps the C ABI onto sendspin-cpp.
#include "sendspin_shim.h"

#include <cstddef>
#include <memory>
#include <optional>
#include <string>
#include <vector>

#include "sendspin/client.h"
#include "sendspin/controller_role.h"
#include "sendspin/metadata_role.h"
#include "sendspin/player_role.h"
#include "sendspin/visualizer_role.h"

namespace ss = ::sendspin;

// The enum values of the header are the library's.
static_assert(uint8_t(ss::SendspinCodecFormat::FLAC) == SS_CODEC_FLAC);
static_assert(uint8_t(ss::SendspinCodecFormat::OPUS) == SS_CODEC_OPUS);
static_assert(uint8_t(ss::SendspinCodecFormat::PCM) == SS_CODEC_PCM);
static_assert(uint8_t(ss::SendspinCodecFormat::UNSUPPORTED) == SS_CODEC_UNSUPPORTED);
static_assert(uint8_t(ss::VisualizerDataType::BEAT) == SS_VIS_BEAT);
static_assert(uint8_t(ss::VisualizerDataType::LOUDNESS) == SS_VIS_LOUDNESS);
static_assert(uint8_t(ss::VisualizerDataType::F_PEAK) == SS_VIS_F_PEAK);
static_assert(uint8_t(ss::VisualizerDataType::SPECTRUM) == SS_VIS_SPECTRUM);
static_assert(uint8_t(ss::VisualizerDataType::PEAK) == SS_VIS_PEAK);
static_assert(uint8_t(ss::VisualizerSpectrumScale::MEL) == SS_SCALE_MEL);
static_assert(uint8_t(ss::VisualizerSpectrumScale::LOG) == SS_SCALE_LOG);
static_assert(uint8_t(ss::VisualizerSpectrumScale::LIN) == SS_SCALE_LIN);
static_assert(uint8_t(ss::SendspinControllerCommand::PLAY) == SS_CMD_PLAY);
static_assert(uint8_t(ss::SendspinControllerCommand::PAUSE) == SS_CMD_PAUSE);
static_assert(uint8_t(ss::SendspinControllerCommand::STOP) == SS_CMD_STOP);
static_assert(uint8_t(ss::SendspinControllerCommand::NEXT) == SS_CMD_NEXT);
static_assert(uint8_t(ss::SendspinControllerCommand::PREVIOUS) == SS_CMD_PREVIOUS);
static_assert(uint8_t(ss::SendspinPlaybackState::PLAYING) == SS_PLAYBACK_PLAYING);
static_assert(uint8_t(ss::SendspinPlaybackState::STOPPED) == SS_PLAYBACK_STOPPED);
static_assert(uint8_t(ss::LogLevel::DEBUG) == SS_LOG_DEBUG);
static_assert(uint8_t(ss::SendspinPairAbortReason::UNKNOWN) == 5);

// Layout shared with src/ffi.rs, which asserts the same numbers (P = pointer size).
constexpr size_t P = sizeof(void *);
static_assert(sizeof(size_t) == P);
static_assert(sizeof(ss_str) == 2 * P && offsetof(ss_str, len) == P);
static_assert(sizeof(ss_format) == 8 && offsetof(ss_format, codec) == 4 && offsetof(ss_format, channels) == 5 &&
              offsetof(ss_format, bit_depth) == 6);
static_assert(offsetof(ss_config, product_name) == 2 * P && offsetof(ss_config, manufacturer) == 4 * P &&
              offsetof(ss_config, software_version) == 6 * P && offsetof(ss_config, mac) == 8 * P &&
              offsetof(ss_config, pairing_psk_location) == 10 * P && offsetof(ss_config, formats) == 12 * P &&
              offsetof(ss_config, n_formats) == 13 * P && offsetof(ss_config, vis_types) == 14 * P &&
              offsetof(ss_config, n_vis_types) == 15 * P && offsetof(ss_config, vis_buffer_capacity) == 16 * P &&
              offsetof(ss_config, vis_display_offset_ms) == 17 * P && offsetof(ss_config, port) == 17 * P + 4 &&
              offsetof(ss_config, vis_rate_max) == 17 * P + 6 && offsetof(ss_config, vis_f_min) == 17 * P + 8 &&
              offsetof(ss_config, vis_f_max) == 17 * P + 10 && offsetof(ss_config, vis_bins) == 17 * P + 12 &&
              offsetof(ss_config, vis_scale) == 17 * P + 13 &&
              offsetof(ss_config, pairing_code_display_digits) == 17 * P + 14 &&
              offsetof(ss_config, unpaired_access) == 17 * P + 15 && sizeof(ss_config) == 17 * P + 16);
static_assert(sizeof(ss_callbacks) == 25 * P);
static_assert(offsetof(ss_callbacks, group_update) == 0 && offsetof(ss_callbacks, time_sync) == P &&
              offsetof(ss_callbacks, pairing_started) == 2 * P && offsetof(ss_callbacks, pairing_succeeded) == 3 * P &&
              offsetof(ss_callbacks, pairing_failed) == 4 * P && offsetof(ss_callbacks, trust_changed) == 5 * P &&
              offsetof(ss_callbacks, display_pairing_code) == 6 * P &&
              offsetof(ss_callbacks, clear_pairing_code) == 7 * P &&
              offsetof(ss_callbacks, open_pairing_window) == 8 * P &&
              offsetof(ss_callbacks, close_pairing_window) == 9 * P && offsetof(ss_callbacks, stream_start) == 10 * P &&
              offsetof(ss_callbacks, stream_end) == 11 * P && offsetof(ss_callbacks, volume_changed) == 12 * P &&
              offsetof(ss_callbacks, mute_changed) == 13 * P &&
              offsetof(ss_callbacks, output_delay_changed) == 14 * P &&
              offsetof(ss_callbacks, controller_state) == 15 * P && offsetof(ss_callbacks, metadata) == 16 * P &&
              offsetof(ss_callbacks, visualizer_stream_start) == 17 * P &&
              offsetof(ss_callbacks, visualizer_stream_end) == 18 * P && offsetof(ss_callbacks, load_blob) == 19 * P &&
              offsetof(ss_callbacks, save_blob) == 20 * P && offsetof(ss_callbacks, audio_write) == 21 * P &&
              offsetof(ss_callbacks, loudness) == 22 * P && offsetof(ss_callbacks, beat) == 23 * P &&
              offsetof(ss_callbacks, spectrum) == 24 * P);

struct ss_blob {
  std::optional<std::vector<uint8_t>> value;
};

namespace {

std::string str(ss_str s) { return s.len ? std::string(s.ptr, s.len) : std::string(); }

// Every listener and provider of the client. The callbacks are Rust functions and never unwind.
// The overrides the library calls on its main loop thread run inside ss_loop / ss_start /
// ss_destroy, whose catch(...) takes anything they throw (e.g. ss_blob's allocation). The ones on
// the sync task (on_audio_write) and the drain thread (on_loudness, on_beat, on_spectrum) only
// pass pointers and integers on and do not throw.
class Bridge final : public ss::SendspinClientListener,
                     public ss::PlayerRoleListener,
                     public ss::MetadataRoleListener,
                     public ss::ControllerRoleListener,
                     public ss::VisualizerRoleListener,
                     public ss::SendspinPersistenceProvider,
                     public ss::SendspinNetworkProvider {
 public:
  Bridge(const ss_callbacks &cb, void *user) : cb_(cb), user_(user) {}

  ss::PlayerRole *player = nullptr;

  // --- SendspinClientListener (main loop) ---
  void on_group_update(const ss::GroupUpdateObject &g) override {
    const int8_t state = g.playback_state ? int8_t(*g.playback_state) : int8_t(SS_PLAYBACK_ABSENT);
    if (g.group_name) cb_.group_update(user_, state, g.group_name->data(), g.group_name->size());
    else cb_.group_update(user_, state, nullptr, 0);
  }
  void on_time_sync_updated(float error) override { cb_.time_sync(user_, error); }
  void on_pairing_started(const std::string &id) override { cb_.pairing_started(user_, id.data(), id.size()); }
  void on_pairing_succeeded(const std::string &id) override { cb_.pairing_succeeded(user_, id.data(), id.size()); }
  void on_pairing_failed(const std::string &id, ss::SendspinPairAbortReason r) override {
    cb_.pairing_failed(user_, id.data(), id.size(), uint8_t(r));
  }
  void on_trust_changed(ss::ConnectionTrust t) override { cb_.trust_changed(user_, t == ss::ConnectionTrust::USER); }
  void on_display_pairing_code(const std::string &code, ss::SendspinPairingCodeFormat) override {
    cb_.display_pairing_code(user_, code.data(), code.size());
  }
  void on_clear_pairing_code() override { cb_.clear_pairing_code(user_); }
  void on_open_pairing_window() override { cb_.open_pairing_window(user_); }
  void on_close_pairing_window() override { cb_.close_pairing_window(user_); }

  // --- PlayerRoleListener ---
  size_t on_audio_write(uint8_t *data, size_t length, uint32_t timeout_ms) override {  // sync task
    uint32_t played = 0;
    int64_t finish_us = 0;
    const size_t n = cb_.audio_write(user_, data, length, timeout_ms, &played, &finish_us);
    if (played && player) player->notify_audio_played(played, finish_us);
    return n;
  }
  void on_stream_start() override {
    const auto &p = player->get_current_stream_params();
    if (!p.is_complete()) return cb_.stream_start(user_, false, SS_CODEC_UNSUPPORTED, 0, 0, 0);
    cb_.stream_start(user_, true, uint8_t(*p.codec), *p.sample_rate, *p.channels, *p.bit_depth);
  }
  void on_stream_end() override { cb_.stream_end(user_); }
  void on_volume_changed(uint8_t v) override { cb_.volume_changed(user_, v); }
  void on_mute_changed(bool m) override { cb_.mute_changed(user_, m); }
  void on_output_delay_changed(uint16_t ms) override { cb_.output_delay_changed(user_, ms); }

  // --- ControllerRoleListener ---
  void on_controller_state(const ss::ServerStateControllerObject &s) override {
    cb_.controller_state(user_, s.supported_commands.size(), s.volume, s.muted);
  }

  // --- MetadataRoleListener ---
  void on_metadata(const ss::ServerMetadataStateObject &m) override {
    cb_.metadata(user_, m.artist ? m.artist->data() : nullptr, m.artist ? m.artist->size() : 0,
                 m.title ? m.title->data() : nullptr, m.title ? m.title->size() : 0);
  }

  // --- VisualizerRoleListener: data on the drain thread, stream start / end on the main loop ---
  void on_loudness(int64_t, uint16_t loudness) override { cb_.loudness(user_, loudness); }
  void on_beat(int64_t, bool) override { cb_.beat(user_); }
  void on_spectrum(int64_t, const std::vector<uint16_t> &bins) override {
    cb_.spectrum(user_, bins.data(), bins.size());
  }
  void on_visualizer_stream_start(const ss::ServerVisualizerStreamObject &s) override {
    cb_.visualizer_stream_start(user_, s.types.size(), s.rate_max);
  }
  void on_visualizer_stream_end() override { cb_.visualizer_stream_end(user_); }

  // --- SendspinPersistenceProvider (main loop) ---
  std::optional<std::vector<uint8_t>> load_blob(const std::string &key) override {
    ss_blob out;
    if (!cb_.load_blob(user_, key.data(), key.size(), &out)) return std::nullopt;
    return std::move(out.value);
  }
  bool save_blob(const std::string &key, const uint8_t *data, size_t len) override {
    return cb_.save_blob(user_, key.data(), key.size(), data, len);
  }

  // --- SendspinNetworkProvider ---
  // The WebSocket server binds 0.0.0.0, so it is usable whenever any interface is up.
  bool is_network_ready() override { return true; }

 private:
  const ss_callbacks cb_;
  void *const user_;
};

ss::SendspinClientConfig client_config(const ss_config &c) {
  ss::SendspinClientConfig k;
  k.name = str(c.name);
  k.product_name = str(c.product_name);
  k.manufacturer = str(c.manufacturer);
  k.software_version = str(c.software_version);
  if (c.mac.len) k.mac_address = str(c.mac);
  k.server_port = c.port;
  if (c.pairing_code_display_digits) {
    k.pairing_code_out_channels = {ss::SendspinPairingCodeChannel::DISPLAY};
    k.pairing_code_formats = {ss::SendspinPairingCodeFormat::DIGITS};
  }
  if (c.pairing_psk_location.len) k.pairing_psk_locations = {str(c.pairing_psk_location)};
  return k;
}

}  // namespace

// The bridge is declared first: the client (and its threads) must be gone before it.
struct ss_client {
  ss_client(const ss_config &cfg, const ss_callbacks &cb, void *user) : bridge(cb, user), client(client_config(cfg)) {}
  Bridge bridge;
  ss::SendspinClient client;
  ss::ControllerRole *controller = nullptr;
};

extern "C" {

void ss_set_log_level(uint8_t level) {
  try {
    ss::SendspinClient::set_log_level(ss::LogLevel(level));
  } catch (...) {
  }
}

int64_t ss_now_us(void) {
  try {
    return ss::SendspinClient::now_us();
  } catch (...) {
    return 0;
  }
}

ss_client *ss_create(const ss_config *cfg, const ss_callbacks *cb, void *user) {
  try {
    auto c = std::make_unique<ss_client>(*cfg, *cb, user);
    ss::PlayerRoleConfig pc;
    for (size_t i = 0; i < cfg->n_formats; i++) {
      const ss_format &f = cfg->formats[i];
      pc.audio_formats.push_back({ss::SendspinCodecFormat(f.codec), f.channels, f.sample_rate, f.bit_depth});
    }
    c->bridge.player = &c->client.add_player(std::move(pc));
    c->bridge.player->set_listener(&c->bridge);
    c->client.add_metadata().set_listener(&c->bridge);
    c->controller = &c->client.add_controller();
    c->controller->set_listener(&c->bridge);
    ss::VisualizerRoleConfig vc;
    vc.support.buffer_capacity = cfg->vis_buffer_capacity;
    for (size_t i = 0; i < cfg->n_vis_types; i++) vc.stream.types.push_back(ss::VisualizerDataType(cfg->vis_types[i]));
    vc.stream.rate_max = cfg->vis_rate_max;
    vc.stream.spectrum = ss::VisualizerSpectrumConfig{.n_disp_bins = cfg->vis_bins,
                                                      .scale = ss::VisualizerSpectrumScale(cfg->vis_scale),
                                                      .f_min = cfg->vis_f_min,
                                                      .f_max = cfg->vis_f_max};
    vc.display_offset_ms = cfg->vis_display_offset_ms;
    c->client.add_visualizer(std::move(vc)).set_listener(&c->bridge);
    c->client.set_listener(&c->bridge);
    c->client.set_network_provider(&c->bridge);
    c->client.set_persistence_provider(&c->bridge);
    c->client.set_unpaired_access_enabled(cfg->unpaired_access);
    return c.release();
  } catch (...) {
    return nullptr;
  }
}

void ss_destroy(ss_client *c) {
  if (!c) return;
  try {
    if (c->client.is_started()) c->client.stop();
  } catch (...) {
  }
  try {
    delete c;
  } catch (...) {
  }
}

int ss_start(ss_client *c) {
  try {
    return c->client.start() ? 1 : 0;
  } catch (...) {
    return -1;
  }
}

bool ss_loop(ss_client *c) {
  try {
    c->client.loop();
    return true;
  } catch (...) {
    return false;
  }
}

bool ss_is_connected(const ss_client *c) {
  try {
    return c->client.is_connected();
  } catch (...) {
    return false;
  }
}

bool ss_server_info(const ss_client *c, ss_put put, void *name_dst, void *id_dst) {
  try {
    auto info = c->client.get_server_information();
    if (!info) return false;
    put(name_dst, info->name.data(), info->name.size());
    put(id_dst, info->server_id.data(), info->server_id.size());
    return true;
  } catch (...) {
    return false;
  }
}

bool ss_client_id(const ss_client *c, ss_put put, void *dst) {
  try {
    const std::string &id = c->client.client_id();
    put(dst, id.data(), id.size());
    return true;
  } catch (...) {
    return false;
  }
}

bool ss_pairing_token(const ss_client *c, ss_put put, void *dst) {
  try {
    auto tok = c->client.pairing_token();
    if (!tok) return false;
    put(dst, tok->data(), tok->size());
    return true;
  } catch (...) {
    return false;
  }
}

void ss_player_set_output_delay_adjustable(ss_client *c, bool adjustable) {
  try {
    c->bridge.player->set_output_delay_adjustable(adjustable);
  } catch (...) {
  }
}

uint8_t ss_player_volume(const ss_client *c) {
  try {
    return c->bridge.player->get_volume();
  } catch (...) {
    return 0;
  }
}

bool ss_player_muted(const ss_client *c) {
  try {
    return c->bridge.player->get_muted();
  } catch (...) {
    return false;
  }
}

void ss_player_update_volume(ss_client *c, uint8_t volume) {
  try {
    c->bridge.player->update_volume(volume);
  } catch (...) {
  }
}

void ss_player_update_muted(ss_client *c, bool muted) {
  try {
    c->bridge.player->update_muted(muted);
  } catch (...) {
  }
}

bool ss_controller_send(ss_client *c, uint8_t command) {
  try {
    return c->controller->send_command({.command = ss::SendspinControllerCommand(command)});
  } catch (...) {
    return false;
  }
}

void ss_leave(ss_client *c) {
  try {
    c->client.leave();
  } catch (...) {
  }
}

void ss_confirm_pairing_window(ss_client *c) {
  try {
    c->client.confirm_pairing_window();
  } catch (...) {
  }
}

bool ss_blob_set(ss_blob *blob, const uint8_t *data, size_t len) {
  try {
    blob->value.emplace(data, data + len);
    return true;
  } catch (...) {
    return false;
  }
}

}  // extern "C"
