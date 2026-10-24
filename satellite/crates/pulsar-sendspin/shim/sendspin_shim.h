// C ABI over sendspin-cpp's SendspinClient with the player, metadata, controller and visualizer
// roles, for the Rust service (src/ffi.rs mirrors every declaration here; both sides assert the
// struct layouts). No C++ exception leaves an ss_* function: each one catches everything and
// returns its failure value.
//
// Threads (the library's, see sendspin/client.h and the role headers):
//  - main loop thread = the thread calling ss_loop(); also every callback fired from inside
//    ss_start(), ss_destroy() (the client's stop()) and ss_loop(). All ss_* functions except
//    ss_now_us() and ss_set_log_level() belong to it;
//  - the player's sync task: audio_write;
//  - the visualizer's drain thread: loudness, beat, spectrum.
// Strings are passed as pointer + length, without a terminating NUL; a NULL pointer means absent
// where a callback says so.
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct ss_client ss_client;
// Storage for one persistence blob being loaded (filled with ss_blob_set()).
typedef struct ss_blob ss_blob;

typedef struct ss_str {
  const char *ptr;
  size_t len;
} ss_str;

// sendspin::SendspinCodecFormat
enum { SS_CODEC_FLAC = 0, SS_CODEC_OPUS = 1, SS_CODEC_PCM = 2, SS_CODEC_UNSUPPORTED = 3 };
// sendspin::VisualizerDataType
enum { SS_VIS_BEAT = 0, SS_VIS_LOUDNESS = 1, SS_VIS_F_PEAK = 2, SS_VIS_SPECTRUM = 3, SS_VIS_PEAK = 4 };
// sendspin::VisualizerSpectrumScale
enum { SS_SCALE_MEL = 0, SS_SCALE_LOG = 1, SS_SCALE_LIN = 2 };
// sendspin::SendspinControllerCommand (the transport subset)
enum { SS_CMD_PLAY = 0, SS_CMD_PAUSE = 1, SS_CMD_STOP = 2, SS_CMD_NEXT = 3, SS_CMD_PREVIOUS = 4 };
// sendspin::SendspinPlaybackState; group_update() passes SS_PLAYBACK_ABSENT without one
enum { SS_PLAYBACK_ABSENT = -1, SS_PLAYBACK_PLAYING = 0, SS_PLAYBACK_STOPPED = 1 };
// sendspin::LogLevel
enum { SS_LOG_NONE = 0, SS_LOG_ERROR = 1, SS_LOG_WARN = 2, SS_LOG_INFO = 3, SS_LOG_DEBUG = 4 };
// pairing_failed() reasons are sendspin::SendspinPairAbortReason values (ATTEMPT_TIMEOUT = 0 ..
// UNKNOWN = 5).

// One sendspin::AudioSupportedFormatObject.
typedef struct ss_format {
  uint32_t sample_rate;
  uint8_t codec;  // SS_CODEC_*
  uint8_t channels;
  uint8_t bit_depth;
} ss_format;

typedef struct ss_config {
  ss_str name;
  ss_str product_name;
  ss_str manufacturer;
  ss_str software_version;
  ss_str mac;                   // empty: the library detects it
  ss_str pairing_psk_location;  // pairing_psk_locations hint; empty: none
  const ss_format *formats;     // player formats, preferred first
  size_t n_formats;
  const uint8_t *vis_types;  // SS_VIS_*
  size_t n_vis_types;
  size_t vis_buffer_capacity;
  int32_t vis_display_offset_ms;
  uint16_t port;
  uint16_t vis_rate_max;
  uint16_t vis_f_min;
  uint16_t vis_f_max;
  uint8_t vis_bins;
  uint8_t vis_scale;  // SS_SCALE_*
  // Dynamic pairing code on the DISPLAY channel in the DIGITS format.
  bool pairing_code_display_digits;
  bool unpaired_access;
} ss_config;

// Every entry must be set. None of them may call an ss_* function except ss_blob_set().
typedef struct ss_callbacks {
  // Main loop thread.
  void (*group_update)(void *user, int8_t playback_state, const char *name, size_t name_len);  // name NULL = absent
  void (*time_sync)(void *user, float error_us);
  void (*pairing_started)(void *user, const char *server_id, size_t len);
  void (*pairing_succeeded)(void *user, const char *server_id, size_t len);
  void (*pairing_failed)(void *user, const char *server_id, size_t len, uint8_t reason);
  void (*trust_changed)(void *user, bool paired);
  void (*display_pairing_code)(void *user, const char *code, size_t len);
  void (*clear_pairing_code)(void *user);
  void (*open_pairing_window)(void *user);
  void (*close_pairing_window)(void *user);
  // complete = false: the stream parameters are incomplete (the other values are then 0).
  void (*stream_start)(void *user, bool complete, uint8_t codec, uint32_t rate, uint8_t channels, uint8_t bits);
  void (*stream_end)(void *user);
  void (*volume_changed)(void *user, uint8_t volume);
  void (*mute_changed)(void *user, bool muted);
  void (*output_delay_changed)(void *user, uint16_t delay_ms);
  void (*controller_state)(void *user, size_t n_commands, uint8_t volume, bool muted);
  void (*metadata)(void *user, const char *artist, size_t artist_len, const char *title, size_t title_len);  // NULL = absent
  void (*visualizer_stream_start)(void *user, size_t n_types, uint16_t rate_max);
  void (*visualizer_stream_end)(void *user);
  // Persistence provider (main loop thread): load_blob returns true and fills `out` when the key
  // is stored; save_blob returns true once the blob is durable.
  bool (*load_blob)(void *user, const char *key, size_t key_len, ss_blob *out);
  bool (*save_blob)(void *user, const char *key, size_t key_len, const uint8_t *data, size_t len);
  // Player sync task: PCM out, may block up to timeout_ms. Returns the bytes consumed (whole
  // frames); a non-zero *played_frames is reported with notify_audio_played(*played_frames,
  // *finish_us) after the call.
  size_t (*audio_write)(void *user, const uint8_t *data, size_t len, uint32_t timeout_ms, uint32_t *played_frames,
                        int64_t *finish_us);
  // Visualizer drain thread.
  void (*loudness)(void *user, uint16_t loudness);
  void (*beat)(void *user);
  void (*spectrum)(void *user, const uint16_t *bins, size_t n);
} ss_callbacks;

// Receives one string from the shim (copied by the callee during the call).
typedef void (*ss_put)(void *dst, const char *s, size_t len);

// Any thread.
void ss_set_log_level(uint8_t level);  // SS_LOG_*
int64_t ss_now_us(void);               // the library's clock (CLOCK_MONOTONIC_RAW on Linux)

// Builds the client with every role, its listeners and providers. `cb` is copied; `user` must
// outlive the client. NULL on failure.
ss_client *ss_create(const ss_config *cfg, const ss_callbacks *cb, void *user);
// Stops a started client (goodbye to the server, joins the library's threads), then frees it.
void ss_destroy(ss_client *c);
int ss_start(ss_client *c);   // 1 started, 0 failed, -1 the library threw
bool ss_loop(ss_client *c);   // false: the library threw
bool ss_is_connected(const ss_client *c);
// false without an active connection; otherwise the server's name and id go to `put`.
bool ss_server_info(const ss_client *c, ss_put put, void *name_dst, void *id_dst);
bool ss_client_id(const ss_client *c, ss_put put, void *dst);
bool ss_pairing_token(const ss_client *c, ss_put put, void *dst);  // false: none
void ss_player_set_output_delay_adjustable(ss_client *c, bool adjustable);
uint8_t ss_player_volume(const ss_client *c);
bool ss_player_muted(const ss_client *c);
void ss_player_update_volume(ss_client *c, uint8_t volume);
void ss_player_update_muted(ss_client *c, bool muted);
bool ss_controller_send(ss_client *c, uint8_t command);  // SS_CMD_*
void ss_leave(ss_client *c);
void ss_confirm_pairing_window(ss_client *c);
// Inside load_blob only: stores the loaded bytes. false: out of memory.
bool ss_blob_set(ss_blob *blob, const uint8_t *data, size_t len);

#ifdef __cplusplus
}
#endif
