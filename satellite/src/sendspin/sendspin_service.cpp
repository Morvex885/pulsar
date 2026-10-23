#include "sendspin/sendspin_service.h"

#include <alsa/asoundlib.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cmath>
#include <cstring>
#include <fstream>
#include <mutex>
#include <optional>
#include <vector>

#include "audio/music_frame.h"
#include "audio/alsa_abi.h"
#include "core/log.h"
#include "sendspin/client.h"
#include "sendspin/controller_role.h"
#include "sendspin/metadata_role.h"
#include "sendspin/player_role.h"
#include "sendspin/visualizer_role.h"

namespace cs {

namespace ss = ::sendspin;

namespace {

constexpr const char *kPcm = "speakers";
constexpr int64_t kLoopTickMs = 10;        // SendspinClient::loop() cadence (integration guide)
constexpr int kWaitSliceMs = 20;           // longest snd_pcm_wait() slice inside one write
constexpr float kGainRampSec = 0.08f;      // full-scale duck ramp duration
constexpr int64_t kStatsPeriodUs = 10000000;
constexpr int64_t kReopenRetryUs = 1000000;

std::string hex_tail(const std::string &s, size_t n) { return s.size() > n ? s.substr(0, n) + "…" : s; }

const char *abort_reason(ss::SendspinPairAbortReason r) {
  switch (r) {
    case ss::SendspinPairAbortReason::ATTEMPT_TIMEOUT: return "attempt_timeout";
    case ss::SendspinPairAbortReason::CONCURRENT_ATTEMPT: return "concurrent_attempt";
    case ss::SendspinPairAbortReason::METHOD_NOT_SUPPORTED: return "method_not_supported";
    case ss::SendspinPairAbortReason::PAIRING_CODE_MISMATCH: return "pairing_code_mismatch";
    case ss::SendspinPairAbortReason::USER_CANCELLED: return "user_cancelled";
    default: return "unknown";
  }
}

// ---------------------------------------------------------------------------------------------
// Persistence: one file per library key under the state directory, written atomically.
// Every call comes from the library's main loop (= our cs::Loop thread).
class FilePersistence : public ss::SendspinPersistenceProvider {
 public:
  explicit FilePersistence(std::string dir) : dir_(std::move(dir)) {}

  bool ensure_dir() {
    if (mkdir(dir_.c_str(), 0700) != 0 && errno != EEXIST) {
      LOGE("sendspin: cannot create %s: %s", dir_.c_str(), strerror(errno));
      return false;
    }
    return true;
  }

  std::optional<std::vector<uint8_t>> load_blob(const std::string &key) override {
    std::ifstream f(path(key), std::ios::binary);
    if (!f) return std::nullopt;
    std::vector<uint8_t> v((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
    if (f.bad()) return std::nullopt;
    return v;
  }

  bool save_blob(const std::string &key, const uint8_t *data, size_t len) override {
    const std::string p = path(key), tmp = p + ".tmp";
    int fd = ::open(tmp.c_str(), O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    if (fd < 0) {
      LOGW("sendspin: cannot write %s: %s", tmp.c_str(), strerror(errno));
      return false;
    }
    size_t off = 0;
    while (off < len) {
      ssize_t n = ::write(fd, data + off, len - off);
      if (n < 0 && errno == EINTR) continue;
      if (n <= 0) break;
      off += size_t(n);
    }
    bool ok = off == len && fsync(fd) == 0;
    ::close(fd);
    if (!ok || rename(tmp.c_str(), p.c_str()) != 0) {
      LOGW("sendspin: cannot store %s: %s", key.c_str(), strerror(errno));
      unlink(tmp.c_str());
      return false;
    }
    int dfd = ::open(dir_.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (dfd >= 0) {
      fsync(dfd);
      ::close(dfd);
    }
    return true;
  }

 private:
  std::string path(const std::string &key) const { return dir_ + "/" + key; }
  std::string dir_;
};

class AlwaysNetwork : public ss::SendspinNetworkProvider {
 public:
  // The WebSocket server binds 0.0.0.0, so it is usable whenever any interface is up.
  bool is_network_ready() override { return true; }
};

// ---------------------------------------------------------------------------------------------
// ALSA sink. configure()/release() run on the cs::Loop thread (stream start/end), write() on the
// library's sync task; the PCM itself is opened lazily by write() so the loop never blocks on it.
// io_mu_ is held by write() for its whole duration, including the snd_pcm_wait slices, so the Loop
// thread never takes it blocking: configure()/release() only record a request (req_mu_, held for a
// few instructions) and apply it right away if the audio thread is idle (try_lock); otherwise the
// audio thread applies it at the next wait slice and abandons the write in progress.
class AlsaSink {
 public:
  std::function<void(uint32_t frames, int64_t finish_us)> on_played;

  ~AlsaSink() {
    std::lock_guard<std::mutex> lk(io_mu_);
    close_locked();
  }

  void configure(uint32_t rate, uint8_t channels, uint8_t bits) { request(Req{Req::CONFIGURE, rate, channels, bits}); }

  // Stream ended: drop what is queued and release the device so dmix is not held while idle.
  void release() { request(Req{Req::RELEASE, 0, 0, 0}); }

  void set_gain(float g) { target_gain_.store(std::clamp(g, 0.f, 1.f)); }

  size_t write(const uint8_t *data, size_t length, uint32_t timeout_ms) {
    size_t r = write_once(data, length, timeout_ms);
    // A request that arrived while io_mu_ was held (request()'s try_lock failed) must not wait for
    // the next write: a RELEASE may be the last event, and the device would stay open.
    if (req_gen_.load() != applied_gen_.load()) {
      std::lock_guard<std::mutex> lk(io_mu_);
      apply_pending_locked();
    }
    return r;
  }

 private:
  size_t write_once(const uint8_t *data, size_t length, uint32_t timeout_ms) {
    const int64_t deadline = ss::SendspinClient::now_us() + int64_t(timeout_ms) * 1000;
    uint32_t frames_done = 0;
    int64_t finish_us = 0;
    size_t bpf = 0;
    bool lost = false;
    uint32_t report = 0;
    {
      std::lock_guard<std::mutex> lk(io_mu_);
      apply_pending_locked();
      bpf = size_t(want_channels_) * (want_bits_ / 8);
      if (!active_ || bpf == 0) return length;  // stale audio after stream end: discard
      if (!pcm_ && !open_locked()) {
        // Device unavailable: discard rather than spin the task; the gap is reported as played
        // with the next timed report so the library's buffered-frames estimate stays right.
        discarded_ += uint32_t(length / bpf);
        return length - length % bpf;
      }
      const size_t frames_total = length / bpf;
      const uint8_t *src = scale_locked(data, frames_total);

      bool first = true;
      while (frames_done < frames_total && pcm_) {
        if (!first && ss::SendspinClient::now_us() >= deadline) break;
        first = false;
        snd_pcm_sframes_t avail = snd_pcm_avail_update(pcm_);
        if (avail < 0) {
          if (!recover_locked(int(avail))) break;
          continue;
        }
        if (avail == 0) {
          int64_t left_ms = (deadline - ss::SendspinClient::now_us()) / 1000;
          if (left_ms <= 0) break;
          int r = snd_pcm_wait(pcm_, int(std::min<int64_t>(left_ms, kWaitSliceMs)));
          if (r < 0 && !recover_locked(r)) break;
          if (req_gen_.load() != applied_gen_.load()) {  // stream restarted/ended: this audio is stale
            apply_pending_locked();
            return length;
          }
          continue;
        }
        snd_pcm_uframes_t chunk = std::min<snd_pcm_uframes_t>(frames_total - frames_done, snd_pcm_uframes_t(avail));
        snd_pcm_sframes_t w = snd_pcm_writei(pcm_, src + size_t(frames_done) * bpf, chunk);
        if (w < 0) {
          if (!recover_locked(int(w))) break;
          continue;
        }
        frames_done += uint32_t(w);
      }
      // Commit the ramp by the frames actually written; an unwritten tail is presented again.
      gain_ = ramp_end_gain(frames_done);

      if (frames_done > 0 && pcm_) {
        // The frames just written finish playing once everything queued ahead of the DAC
        // (our ring + plug/rate converter + dmix ring) has played: now + snd_pcm_delay().
        snd_pcm_sframes_t delay = 0;
        if (snd_pcm_delay(pcm_, &delay) == 0 && delay >= 0) {
          const int64_t now = ss::SendspinClient::now_us();
          finish_us = now + int64_t(delay) * 1000000 / rate_;
          last_delay_frames_ = delay;
          delay_min_ = std::min(delay_min_, delay);
          delay_max_ = std::max(delay_max_, delay);
          if (!first_write_us_) first_write_us_ = now;
        }
      }
      frames_total_ += frames_done;
      maybe_log_stats_locked();
      lost = !pcm_;
      if (lost && frames_done == 0) discarded_ += uint32_t(length / bpf);
      if (finish_us) {
        report = frames_done + discarded_;
        discarded_ = 0;
      }
    }
    if (report && on_played) on_played(report, finish_us);
    if (lost && frames_done == 0) return length - length % bpf;  // device lost mid-write: discard
    return size_t(frames_done) * bpf;
  }

 struct Req {
    enum Kind { NONE, CONFIGURE, RELEASE } kind;
    uint32_t rate;
    uint8_t channels, bits;
  };

  void request(Req r) {
    {
      std::lock_guard<std::mutex> lk(req_mu_);
      req_ = r;
      req_gen_.fetch_add(1);
    }
    std::unique_lock<std::mutex> lk(io_mu_, std::try_to_lock);
    if (lk) apply_pending_locked();
  }

  void apply_pending_locked() {
    Req r;
    {
      std::lock_guard<std::mutex> lk(req_mu_);
      applied_gen_.store(req_gen_.load());
      r = req_;
      req_.kind = Req::NONE;
    }
    if (r.kind == Req::CONFIGURE) apply_configure_locked(r.rate, r.channels, r.bits);
    else if (r.kind == Req::RELEASE) apply_release_locked();
  }

  void apply_configure_locked(uint32_t rate, uint8_t channels, uint8_t bits) {
    if (pcm_ && (rate != rate_ || channels != channels_ || bits != bits_)) close_locked();
    if (pcm_) {  // same format: restart from an empty ring
      snd_pcm_drop(pcm_);
      if (snd_pcm_prepare(pcm_) < 0) close_locked();
    }
    want_rate_ = rate;
    want_channels_ = channels;
    want_bits_ = bits;
    active_ = true;
    open_failed_logged_ = false;
    next_open_us_ = 0;
    discarded_ = 0;
    gain_ = target_gain_.load();  // a new stream starts at the current duck level, no ramp
  }

  void apply_release_locked() {
    active_ = false;
    if (pcm_) {
      LOGI("sendspin: stream summary: %.1f s written to the device, xruns %u", rate_ ? double(frames_total_) / rate_ : 0.0,
           xruns_);
      snd_pcm_drop(pcm_);
    }
    close_locked();
  }

  bool open_locked() {
    const int64_t now = ss::SendspinClient::now_us();
    if (now < next_open_us_) return false;
    snd_pcm_format_t fmt;
    switch (want_bits_) {
      case 16: fmt = SND_PCM_FORMAT_S16_LE; break;
      case 24: fmt = SND_PCM_FORMAT_S24_3LE; break;
      case 32: fmt = SND_PCM_FORMAT_S32_LE; break;
      default:
        LOGE("sendspin: unsupported bit depth %u", want_bits_);
        next_open_us_ = now + kReopenRetryUs;
        return false;
    }
    int err = snd_pcm_open(&pcm_, kPcm, SND_PCM_STREAM_PLAYBACK, 0);
    const char *step = "open";
    snd_pcm_hw_params_t *hw = nullptr;
    snd_pcm_hw_params_alloca(&hw);
    snd_pcm_uframes_t period = 0, buffer = 0;
    if (err >= 0) step = "hw_params_any", err = snd_pcm_hw_params_any(pcm_, hw);
    if (err >= 0) step = "access", err = snd_pcm_hw_params_set_access(pcm_, hw, SND_PCM_ACCESS_RW_INTERLEAVED);
    if (err >= 0) step = "format", err = snd_pcm_hw_params_set_format(pcm_, hw, fmt);
    if (err >= 0) step = "channels", err = snd_pcm_hw_params_set_channels(pcm_, hw, want_channels_);
    if (err >= 0) step = "rate", err = snd_pcm_hw_params_set_rate(pcm_, hw, want_rate_, 0);
    if (err >= 0) step = "hw_params", err = snd_pcm_hw_params(pcm_, hw);
    if (err >= 0) {
      alsa::hw_params_get_period_size(hw, &period, nullptr);
      alsa::hw_params_get_buffer_size(hw, &buffer);
      snd_pcm_sw_params_t *sw = nullptr;
      snd_pcm_sw_params_alloca(&sw);
      step = "sw_params";
      err = snd_pcm_sw_params_current(pcm_, sw);
      // Start as soon as one period is queued so snd_pcm_delay() tracks real playback early.
      if (err >= 0) err = snd_pcm_sw_params_set_start_threshold(pcm_, sw, period ? period : 1);
      if (err >= 0) err = snd_pcm_sw_params_set_avail_min(pcm_, sw, period ? period : 1);
      if (err >= 0) err = snd_pcm_sw_params(pcm_, sw);
    }
    if (err >= 0) step = "prepare", err = snd_pcm_prepare(pcm_);
    if (err < 0) {
      if (!open_failed_logged_)
        LOGE("sendspin: ALSA '%s' %s failed for %u Hz/%u ch/%u bit: %s", kPcm, step, want_rate_, want_channels_,
             want_bits_, snd_strerror(err));
      open_failed_logged_ = true;
      close_locked();
      next_open_us_ = now + kReopenRetryUs;
      return false;
    }
    rate_ = want_rate_;
    channels_ = want_channels_;
    bits_ = want_bits_;
    xruns_ = 0;
    frames_total_ = 0;
    first_write_us_ = 0;
    prev_stats_us_ = 0;
    prev_played_ = 0;
    delay_min_ = INT32_MAX;
    delay_max_ = 0;
    stats_us_ = now;
    LOGI("sendspin: ALSA '%s' open %u Hz/%u ch/%u bit (period %lu, buffer %lu frames = %.0f ms)", kPcm, rate_,
         channels_, bits_, (unsigned long)period, (unsigned long)buffer, buffer * 1000.0 / rate_);
    return true;
  }

  void close_locked() {
    if (!pcm_) return;
    snd_pcm_close(pcm_);
    pcm_ = nullptr;
    LOGI("sendspin: ALSA '%s' closed", kPcm);
  }

  bool recover_locked(int err) {
    if (err == -EINTR || err == -EAGAIN) return true;
    if (err == -EPIPE || err == -ESTRPIPE) {
      xruns_++;
      int r = err == -ESTRPIPE ? snd_pcm_resume(pcm_) : -1;
      if (r < 0) r = snd_pcm_prepare(pcm_);
      if (r >= 0) {
        LOGD("sendspin: ALSA %s recovered", err == -EPIPE ? "underrun" : "suspend");
        return true;
      }
      err = r;
    }
    LOGE("sendspin: ALSA error %s, closing device", snd_strerror(err));
    close_locked();
    next_open_us_ = ss::SendspinClient::now_us() + kReopenRetryUs;
    return false;
  }

  // Gain after `frames` frames of the linear ramp from gain_ toward the target.
  float ramp_end_gain(uint32_t frames) const {
    const float target = target_gain_.load();
    const float step = 1.f / (kGainRampSec * float(rate_ ? rate_ : 48000));
    float d = step * float(frames);
    return gain_ < target ? std::min(target, gain_ + d) : std::max(target, gain_ - d);
  }

  // Applies the (ramping) duck gain; returns the buffer to write.
  const uint8_t *scale_locked(const uint8_t *data, size_t frames) {
    const float target = target_gain_.load();
    if (gain_ == target && target >= 1.f) return data;
    if (bits_ != 16) return data;  // only 16-bit streams are advertised
    scratch_.resize(frames * channels_);
    const int16_t *in = reinterpret_cast<const int16_t *>(data);
    const float step = 1.f / (kGainRampSec * float(rate_));
    float g = gain_;
    for (size_t f = 0; f < frames; f++) {
      if (g < target) g = std::min(target, g + step);
      else if (g > target) g = std::max(target, g - step);
      for (unsigned c = 0; c < channels_; c++) {
        size_t i = f * channels_ + c;
        scratch_[i] = int16_t(std::lrintf(float(in[i]) * g));
      }
    }
    return reinterpret_cast<const uint8_t *>(scratch_.data());
  }

  void maybe_log_stats_locked() {
    if (ss::SendspinClient::now_us() - stats_us_ < kStatsPeriodUs) return;
    log_stats_locked("sink");
  }

  // Called from write() right after snd_pcm_delay(): frames that reached the DAC since the last
  // report (written minus still queued) against the wall-clock time between the two reports. The
  // ratio (ppm) is the net rate the library's sync fed in this interval: server clock vs. DAC
  // clock drift plus soft/hard sync corrections. Delay min/max show how full the device stayed.
  void log_stats_locked(const char *what) {
    const int64_t now = ss::SendspinClient::now_us();
    stats_us_ = now;
    if (!rate_ || !first_write_us_) return;
    const int64_t played = int64_t(frames_total_) - last_delay_frames_;
    const bool interval = prev_stats_us_ != 0;
    const double wall = double(now - (interval ? prev_stats_us_ : first_write_us_)) / 1e6;
    const double dplayed = double(played - (interval ? prev_played_ : 0)) / rate_;
    LOGI("sendspin: %s %u Hz/%u ch: %.1f s total, last %.1f s: %.3f s played (%+.0f ppm), output delay %.1f ms "
         "(min %.1f, max %.1f), xruns %u, duck gain %.2f",
         what, rate_, channels_, double(played) / rate_, wall, dplayed, wall > 0.5 ? (dplayed / wall - 1) * 1e6 : 0.0,
         last_delay_frames_ * 1000.0 / rate_, delay_min_ * 1000.0 / rate_, delay_max_ * 1000.0 / rate_, xruns_, gain_);
    prev_stats_us_ = now;
    prev_played_ = played;
    delay_min_ = INT32_MAX;
    delay_max_ = 0;
  }

  std::mutex io_mu_;  // audio-thread state below, held across write()
  std::mutex req_mu_;
  Req req_{Req::NONE, 0, 0, 0};
  std::atomic<uint32_t> req_gen_{0};
  std::atomic<uint32_t> applied_gen_{0};
  snd_pcm_t *pcm_ = nullptr;
  bool active_ = false;
  uint32_t want_rate_ = 0, rate_ = 0;
  uint8_t want_channels_ = 0, channels_ = 0, want_bits_ = 0, bits_ = 0;
  bool open_failed_logged_ = false;
  int64_t next_open_us_ = 0;
  uint32_t discarded_ = 0;  // frames consumed without reaching the device, not yet reported
  std::atomic<float> target_gain_{1.f};
  float gain_ = 1.f;  // sync task only (under io_mu_)
  std::vector<int16_t> scratch_;
  // stats
  uint64_t frames_total_ = 0;
  unsigned xruns_ = 0;
  snd_pcm_sframes_t last_delay_frames_ = 0, delay_min_ = INT32_MAX, delay_max_ = 0;
  int64_t first_write_us_ = 0, prev_stats_us_ = 0, prev_played_ = 0;
  int64_t stats_us_ = 0;
};

}  // namespace

// -----------------------------------------------------------------------------------------------

struct SendspinService::Impl : ss::SendspinClientListener, ss::PlayerRoleListener, ss::MetadataRoleListener,
                               ss::ControllerRoleListener, ss::VisualizerRoleListener {
  Impl(Loop &l, SendspinConfig c, Hooks h)
      : loop(l), cfg(std::move(c)), hooks(std::move(h)), persistence(cfg.state_dir), client(make_config(cfg)) {}

  static ss::SendspinClientConfig make_config(const SendspinConfig &c) {
    ss::SendspinClientConfig k;
    k.name = c.name;
    k.product_name = c.product_name;
    k.manufacturer = c.manufacturer;
    k.software_version = c.software_version;
    if (!c.mac.empty()) k.mac_address = c.mac;
    k.server_port = c.port;
    // Dynamic pairing code: six digits shown on the 4-digit clock (paged as 3-3 groups).
    k.pairing_code_out_channels = {ss::SendspinPairingCodeChannel::DISPLAY};
    k.pairing_code_formats = {ss::SendspinPairingCodeFormat::DIGITS};
    k.pairing_psk_locations = {"operator"};  // pairing token is printed to the daemon log
    return k;
  }

  Loop &loop;
  SendspinConfig cfg;
  Hooks hooks;
  FilePersistence persistence;
  AlwaysNetwork network;
  AlsaSink sink;
  ss::SendspinClient client;
  ss::PlayerRole *player = nullptr;
  ss::ControllerRole *controller = nullptr;
  uint64_t tick = 0;
  bool streaming = false;
  bool connected = false;
  bool hooks_enabled = true;
  // Visualizer drain thread only.
  float vis_loudness = 0;
  bool vis_beat = false;

  // --- SendspinClientListener (main loop) ---
  void on_group_update(const ss::GroupUpdateObject &g) override {
    LOGI("sendspin: group %s%s%s", g.group_name ? g.group_name->c_str() : "-",
         g.playback_state ? " state=" : "",
         g.playback_state ? (*g.playback_state == ss::SendspinPlaybackState::PLAYING ? "playing" : "stopped") : "");
  }
  void on_time_sync_updated(float error) override { LOGD("sendspin: time sync error %.0f us", error); }
  void on_pairing_started(const std::string &server_id) override {
    LOGI("sendspin: pairing started by server %s", server_id.c_str());
  }
  void on_pairing_succeeded(const std::string &server_id) override {
    LOGI("sendspin: paired with server %s", server_id.c_str());
  }
  void on_pairing_failed(const std::string &server_id, ss::SendspinPairAbortReason r) override {
    LOGW("sendspin: pairing with %s failed: %s", server_id.c_str(), abort_reason(r));
  }
  void on_trust_changed(ss::ConnectionTrust trust) override {
    LOGI("sendspin: connection trust %s", trust == ss::ConnectionTrust::USER ? "paired" : "unpaired");
  }
  void on_display_pairing_code(const std::string &code, ss::SendspinPairingCodeFormat) override {
    LOGI("sendspin: PAIRING CODE %s-%s (enter it in Music Assistant)", code.substr(0, 3).c_str(),
         code.size() > 3 ? code.substr(3).c_str() : "");
    if (hooks_enabled && hooks.on_pairing_code) hooks.on_pairing_code(code);
  }
  void on_clear_pairing_code() override {
    LOGI("sendspin: pairing code withdrawn");
    if (hooks_enabled && hooks.on_pairing_code) hooks.on_pairing_code("");
  }
  void on_open_pairing_window() override {
    LOGI("sendspin: pairing needs confirmation on the device: press the action button");
    if (hooks_enabled && hooks.on_pairing_window) hooks.on_pairing_window(true);
  }
  void on_close_pairing_window() override {
    if (hooks_enabled && hooks.on_pairing_window) hooks.on_pairing_window(false);
  }

  // --- PlayerRoleListener ---
  size_t on_audio_write(uint8_t *data, size_t length, uint32_t timeout_ms) override {  // sync task
    return sink.write(data, length, timeout_ms);
  }
  void on_stream_start() override {
    const auto &p = player->get_current_stream_params();
    if (!p.is_complete()) {
      LOGW("sendspin: stream start without complete parameters");
      return;
    }
    static const char *const kCodec[] = {"flac", "opus", "pcm", "?"};
    LOGI("sendspin: stream start %s %u Hz/%u ch/%u bit", kCodec[std::min<unsigned>(unsigned(*p.codec), 3)],
         *p.sample_rate, *p.channels, *p.bit_depth);
    sink.configure(*p.sample_rate, *p.channels, *p.bit_depth);
    if (!streaming) {
      streaming = true;
      if (hooks_enabled && hooks.on_stream) hooks.on_stream(true);
    }
  }
  void on_stream_end() override {
    sink.release();
    if (streaming) {
      LOGI("sendspin: stream end");
      streaming = false;
      if (hooks_enabled && hooks.on_stream) hooks.on_stream(false);
    }
  }
  void on_volume_changed(uint8_t v) override {
    LOGI("sendspin: server volume %u", v);
    if (hooks_enabled && hooks.on_volume) hooks.on_volume(v / 100.f);
  }
  void on_mute_changed(bool m) override {
    LOGI("sendspin: server mute %d", m);
    if (hooks_enabled && hooks.on_mute) hooks.on_mute(m);
  }
  void on_output_delay_changed(uint16_t ms) override { LOGI("sendspin: output delay %u ms", ms); }

  // --- ControllerRoleListener ---
  void on_controller_state(const ss::ServerStateControllerObject &s) override {
    LOGD("sendspin: controller state: %zu commands, group volume %u, muted %d", s.supported_commands.size(),
         s.volume, s.muted);
  }

  // --- MetadataRoleListener ---
  void on_metadata(const ss::ServerMetadataStateObject &m) override {
    if (m.title || m.artist)
      LOGI("sendspin: now playing: %s - %s", m.artist ? m.artist->c_str() : "?", m.title ? m.title->c_str() : "?");
  }

  // --- VisualizerRoleListener: data callbacks on the library's drain thread ---
  void on_loudness(int64_t, uint16_t loudness) override { vis_loudness = loudness / 65535.f; }
  void on_beat(int64_t, bool) override { vis_beat = true; }
  void on_spectrum(int64_t, const std::vector<uint16_t> &bins) override {
    const size_t n = bins.size();
    if (!n) return;
    MusicFrame f;
    for (size_t b = 0; b < 8; b++) {
      size_t lo = b * n / 8, hi = (b + 1) * n / 8;
      if (n < 8 || hi <= lo) {
        f.bands[b] = bins[std::min(lo, n - 1)] / 65535.f;
        continue;
      }
      uint32_t sum = 0;
      for (size_t i = lo; i < hi; i++) sum += bins[i];
      f.bands[b] = float(sum) / float(hi - lo) / 65535.f;
    }
    f.loudness = vis_loudness;
    f.beat = vis_beat;
    vis_beat = false;
    loop.post([this, f] {
      if (hooks_enabled && hooks.on_music) hooks.on_music(f);
    });
  }
  // --- VisualizerRoleListener: stream start / end on the main loop ---
  void on_visualizer_stream_start(const ss::ServerVisualizerStreamObject &s) override {
    LOGI("sendspin: visualizer stream start, %zu types, %u fps", s.types.size(), s.rate_max);
  }
  void on_visualizer_stream_end() override { LOGI("sendspin: visualizer stream end"); }

  void poll() {
    client.loop();
    bool c = client.is_connected();
    if (c != connected) {
      connected = c;
      if (c) {
        auto info = client.get_server_information();
        LOGI("sendspin: server connected: %s (%s)", info ? info->name.c_str() : "?",
             info ? hex_tail(info->server_id, 12).c_str() : "?");
      } else {
        LOGI("sendspin: server disconnected");
      }
    }
  }
};

SendspinService::SendspinService(Loop &loop, SendspinConfig cfg, Hooks hooks)
    : impl_(new Impl(loop, std::move(cfg), std::move(hooks))) {}

SendspinService::~SendspinService() {
  auto &d = *impl_;
  d.hooks_enabled = false;  // the owner is being torn down
  if (d.tick) d.loop.cancel_timer(d.tick);
  if (d.client.is_started()) d.client.stop();  // goodbye to the server, joins the library threads
}

bool SendspinService::start(float volume, bool muted) {
  auto &d = *impl_;
  ss::SendspinClient::set_log_level(ss::LogLevel::INFO);
  if (!d.persistence.ensure_dir()) LOGW("sendspin: identity will not persist");

  ss::PlayerRoleConfig pc;
  pc.audio_formats = {
      {ss::SendspinCodecFormat::FLAC, 2, 48000, 16},
      {ss::SendspinCodecFormat::FLAC, 2, 44100, 16},
#ifdef SENDSPIN_ENABLE_OPUS
      {ss::SendspinCodecFormat::OPUS, 2, 48000, 16},
#endif
      {ss::SendspinCodecFormat::PCM, 2, 48000, 16},
      {ss::SendspinCodecFormat::PCM, 2, 44100, 16},
  };
  d.player = &d.client.add_player(std::move(pc));
  d.player->set_listener(&d);
  d.client.add_metadata().set_listener(&d);
  d.controller = &d.client.add_controller();
  d.controller->set_listener(&d);
  // 8 log-spaced bands over the range the speaker-reference analyzer covers; ~30 frames/s matches
  // the LED frame period (33 ms). Only ~1/7 of buffer_capacity holds wire data and the server
  // sends frames as far ahead as audio (seconds): 64 KB keeps its flow control from starving them.
  ss::VisualizerRoleConfig vc;
  vc.support.buffer_capacity = 65536;
  vc.stream.types = {ss::VisualizerDataType::SPECTRUM, ss::VisualizerDataType::LOUDNESS,
                     ss::VisualizerDataType::BEAT};
  vc.stream.rate_max = 30;
  vc.stream.spectrum = ss::VisualizerSpectrumConfig{
      .n_disp_bins = 8, .scale = ss::VisualizerSpectrumScale::LOG, .f_min = 40, .f_max = 7600};
  vc.display_offset_ms = 0;
  d.client.add_visualizer(std::move(vc)).set_listener(&d);
  d.client.set_listener(&d);
  d.client.set_network_provider(&d.network);
  d.client.set_persistence_provider(&d.persistence);
  d.sink.on_played = [p = d.player](uint32_t frames, int64_t ts) { p->notify_audio_played(frames, ts); };

  // Unpaired access lets Music Assistant use the speaker after the operator approves it there;
  // turn it off (pairing required) by writing 0 to <state_dir>/unpaired_access.
  bool unpaired = true;
  {
    std::ifstream f(d.cfg.state_dir + "/unpaired_access");
    char c;
    if (f >> c) unpaired = c != '0';
  }
  d.client.set_unpaired_access_enabled(unpaired);

  if (!d.client.start()) {
    LOGE("sendspin: client start failed");
    return false;
  }
  d.player->set_output_delay_adjustable(true);
  d.player->update_volume(uint8_t(std::lround(std::clamp(volume, 0.f, 1.f) * 100)));
  d.player->update_muted(muted);
  LOGI("sendspin: listening on ws://0.0.0.0:%u/sendspin as '%s', client_id %s, unpaired access %s", d.cfg.port,
       d.cfg.name.c_str(), d.client.client_id().c_str(), unpaired ? "on" : "off");
  if (auto tok = d.client.pairing_token()) LOGI("sendspin: pairing token %s", tok->c_str());
  d.tick = d.loop.add_timer(kLoopTickMs, [&d] { d.poll(); }, kLoopTickMs);
  return true;
}

bool SendspinService::streaming() const { return impl_->streaming; }

bool SendspinService::connected() const { return impl_->client.is_connected(); }

bool SendspinService::transport(Transport t) {
  auto &d = *impl_;
  if (!d.controller || !d.client.is_connected()) return false;
  ss::SendspinControllerCommand c = ss::SendspinControllerCommand::PLAY;
  const char *name = "play";
  switch (t) {
    case Transport::PLAY: break;
    case Transport::PAUSE: c = ss::SendspinControllerCommand::PAUSE, name = "pause"; break;
    case Transport::STOP: c = ss::SendspinControllerCommand::STOP, name = "stop"; break;
    case Transport::NEXT: c = ss::SendspinControllerCommand::NEXT, name = "next"; break;
    case Transport::PREVIOUS: c = ss::SendspinControllerCommand::PREVIOUS, name = "previous"; break;
  }
  bool ok = d.controller->send_command({.command = c});
  LOGI("sendspin: transport %s %s", name, ok ? "sent" : "not offered by the server");
  return ok;
}

void SendspinService::set_duck_gain(float gain) { impl_->sink.set_gain(gain); }

void SendspinService::report_volume(float volume, bool muted) {
  auto &d = *impl_;
  if (!d.player || !d.client.is_started()) return;
  uint8_t v = uint8_t(std::lround(std::clamp(volume, 0.f, 1.f) * 100));
  if (v != d.player->get_volume()) d.player->update_volume(v);
  if (muted != d.player->get_muted()) d.player->update_muted(muted);
}

void SendspinService::yield_to_local() {
  auto &d = *impl_;
  if (!d.streaming) return;
  LOGI("sendspin: local playback took over, leaving the group");
  d.client.leave();
}

void SendspinService::confirm_pairing_window() {
  LOGI("sendspin: pairing window confirmed by the operator");
  impl_->client.confirm_pairing_window();
}

}  // namespace cs
