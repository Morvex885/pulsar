#include "audio/frontend.h"

#include <alsa/asoundlib.h>
#include <dlfcn.h>

#include <algorithm>
#include <cmath>
#include <condition_variable>
#include <cstring>

#include "core/log.h"

// Signatures of libyandex_evil.so (recovered by reverse engineering maind, verified on device):
//   VQE functions return a YandexVqeC_Error* (nullptr on success).
//   Spotter functions take a trailing spotter_error** (set on failure).
//   VQE I/O is float in [-1, 1]; mics/speakers are frame-interleaved; Process consumes
//   GetInputChunkSize() frames (Midi: 3072 @ 96 kHz, Lite 2: 1536 @ 48 kHz) and yields
//   GetProcessingFrame() samples (512 @ 16 kHz) per output channel. Output channel types follow
//   voicetech.AudioInputChannelType.
//   Spotter input is s16le interleaved in the channel order passed to spotter_create_multichannel.

namespace cs {
namespace {

constexpr const char *kLibDir = "/system/vendor/quasar/libs/";
constexpr const char *kVqeModels = "/system/vendor/quasar/vqe_models";

// voicetech.AudioInputChannelType (VQE output channel types)
enum : int { RAW_MAIN_MIC_SYNC = 2, VQE_OMNI = 6, VQE_OMNI_WITH_GAIN = 9 };
// Spotter channel type (0..5) -> VQE output feeding it. maind maps spotter types to
// {UNKNOWN(main), RAW_MAIN_MIC_SYNC, VQE_OMNI_WITH_GAIN, RAW_AUX_MIC_SYNC, VQE_OMNI, VQE_BEAMFORMING};
// the cucumber preset has no AUX/BEAMFORMING outputs, so the closest available ones are used.
constexpr int kSpotterToVqe[6] = {VQE_OMNI_WITH_GAIN, RAW_MAIN_MIC_SYNC, VQE_OMNI_WITH_GAIN,
                                  RAW_MAIN_MIC_SYNC,  VQE_OMNI,          VQE_OMNI};
// Spotter events (event_id_to_str). Hits are confirmations; 0/4/7/9 mean "need more data / finished".
enum : int { EV_CONFIRM_1ST = 2, EV_CONFIRM_2ND = 5 };
constexpr unsigned kEventsStopMask = (1u << 0) | (1u << 4) | (1u << 7) | (1u << 9);

using P = void *;

}  // namespace

struct AudioFrontend::Engine {
  void *lib = nullptr;
  // VQE
  P (*cfg_create)(const char *, P *) = nullptr;
  void (*cfg_free)(P) = nullptr;
  int (*cfg_chunk)(P) = nullptr;
  int (*cfg_frame)(P) = nullptr;
  P (*vqe_create)(P, const char *, size_t, const void *, P *) = nullptr;
  void (*vqe_free)(P) = nullptr;
  P (*vqe_process)(P, const float *, size_t, const float *, size_t) = nullptr;
  P (*vqe_out)(P, int, size_t, const float **, size_t *) = nullptr;
  const char *(*vqe_errmsg)(P) = nullptr;
  void (*vqe_errfree)(P) = nullptr;
  // spotter
  P (*sp_conf_read)(const char *, P *) = nullptr;
  void (*sp_conf_free)(P, P *) = nullptr;
  int (*sp_conf_rate)(P, P *) = nullptr;
  long (*sp_conf_chans)(P, long, P *) = nullptr;
  P (*sp_create)(P, int, int, const int *, bool, P *) = nullptr;
  void (*sp_destroy)(P, P *) = nullptr;
  void (*sp_submit)(P, const void *, size_t, P *) = nullptr;
  int (*sp_next)(P, P *) = nullptr;
  const char *(*sp_phrase)(P, P *) = nullptr;
  const char *(*sp_errmsg)(P) = nullptr;
  void (*sp_errfree)(P) = nullptr;

  struct Spotter {
    explicit Spotter(const char *w) : what(w) {}
    const char *what;
    P sp = nullptr;
    std::vector<int> vqe;  // per spotter input channel: VQE output type
    std::vector<int16_t> buf;
    int64_t last_hit_ms = 0;
  };

  P cfg = nullptr, vqe = nullptr;
  int chunk = 0, frame = 0;
  Spotter wake{"wake word"}, cmd{"command"};

  template <class F>
  bool bind(F &f, const char *name) {
    f = reinterpret_cast<F>(dlsym(lib, name));
    if (!f) LOGE("libyandex_evil: missing %s", name);
    return f != nullptr;
  }

  bool load() {
    // libyandex_evil.so needs libtflite.so (same dir); preload it so no LD_LIBRARY_PATH is required.
    std::string dir = kLibDir;
    if (!dlopen((dir + "libtflite.so").c_str(), RTLD_NOW | RTLD_GLOBAL)) LOGW("libtflite: %s", dlerror());
    lib = dlopen((dir + "libyandex_evil.so").c_str(), RTLD_NOW | RTLD_GLOBAL);
    if (!lib) {
      LOGE("dlopen libyandex_evil: %s", dlerror());
      return false;
    }
    return bind(cfg_create, "YandexVqeC_Config_CreateFromPreset") && bind(cfg_free, "YandexVqeC_Config_Free") &&
           bind(cfg_chunk, "YandexVqeC_Config_GetInputChunkSize") &&
           bind(cfg_frame, "YandexVqeC_Config_GetProcessingFrame") && bind(vqe_create, "YandexVqeC_Create") &&
           bind(vqe_free, "YandexVqeC_Free") && bind(vqe_process, "YandexVqeC_Process") &&
           bind(vqe_out, "YandexVqeC_GetOutputChannelData") && bind(vqe_errmsg, "YandexVqeC_Error_GetMessage") &&
           bind(vqe_errfree, "YandexVqeC_Error_Free") && bind(sp_conf_read, "spotter_conf_read") &&
           bind(sp_conf_free, "spotter_conf_free") && bind(sp_conf_rate, "spotter_conf_native_sample_rate") &&
           bind(sp_conf_chans, "spotter_conf_get_required_channel_count") &&
           bind(sp_create, "spotter_create_multichannel") && bind(sp_destroy, "spotter_destroy") &&
           bind(sp_submit, "spotter_submit_raw_data") && bind(sp_next, "spotter_get_next_event") &&
           bind(sp_phrase, "spotter_get_current_phrase") && bind(sp_errmsg, "spotter_error_get_message") &&
           bind(sp_errfree, "spotter_error_free");
  }

  bool vqe_check(P err, const char *what) {
    if (!err) return true;
    LOGE("%s: %s", what, vqe_errmsg(err));
    vqe_errfree(err);
    return false;
  }
  bool sp_check(P err, const char *what) {
    if (!err) return true;
    LOGE("%s: %s", what, sp_errmsg(err));
    sp_errfree(err);
    return false;
  }

  bool init_vqe(const std::string &preset) {
    if (!vqe_check(cfg_create(preset.c_str(), &cfg), "VQE config")) return false;
    chunk = cfg_chunk(cfg);
    frame = cfg_frame(cfg);
    if (!vqe_check(vqe_create(cfg, kVqeModels, 0, nullptr, &vqe), "VQE create")) return false;
    LOGI("VQE '%s': %d input frames -> %d output samples per chunk", preset.c_str(), chunk, frame);
    return true;
  }

  void free_spotter(Spotter &s) {
    if (!s.sp) return;
    P e = nullptr;
    sp_destroy(s.sp, &e);
    sp_check(e, "spotter_destroy");
    s.sp = nullptr;
  }

  // main_vqe: VQE output for the spotter's main channel (type 0). Measured offline on the device:
  // the wake-word model works with VQE_OMNI_WITH_GAIN, the command model only with VQE_OMNI
  // (10/14 synthetic commands vs 0/14 with the AGC'd channel).
  bool init_spotter(Spotter &s, const std::string &dir, int main_vqe) {
    free_spotter(s);
    P e = nullptr;
    P conf = sp_conf_read(dir.c_str(), &e);
    if (!sp_check(e, "spotter_conf_read") || !conf) return false;
    std::vector<int> types;
    s.vqe.clear();
    for (int t = 0; t < 6; t++) {
      long n = sp_conf_chans(conf, t, &e);
      if (!sp_check(e, "spotter_conf_get_required_channel_count")) n = 0;
      for (long i = 0; i < n && n < 8; i++) {
        types.push_back(t);
        s.vqe.push_back(t == 0 ? main_vqe : kSpotterToVqe[t]);
      }
    }
    int rate = sp_conf_rate(conf, &e);
    sp_check(e, "spotter_conf_native_sample_rate");
    s.sp = sp_create(conf, 16000, (int)types.size(), types.data(), false, &e);
    bool ok = sp_check(e, "spotter_create_multichannel") && s.sp;
    sp_conf_free(conf, &e);
    sp_check(e, "spotter_conf_free");
    if (ok) {
      std::string layout;
      for (size_t i = 0; i < types.size(); i++)
        layout += (i ? "," : "") + std::to_string(types[i]) + "<-vqe" + std::to_string(s.vqe[i]);
      LOGI("%s spotter %s: native rate %d, channels [%s]", s.what, dir.c_str(), rate, layout.c_str());
    }
    return ok;
  }

  // Feeds the current VQE chunk to a spotter; returns the phrases it confirmed.
  std::vector<std::string> feed(Spotter &s) {
    std::vector<std::string> hits;
    const size_t nch = s.vqe.size();
    if (!s.sp || !nch) return hits;
    const float *src[8];
    size_t n = frame;
    for (size_t k = 0; k < nch; k++) {
      size_t len = 0;
      if (!vqe_check(vqe_out(vqe, s.vqe[k], 0, &src[k], &len), "VQE output")) return hits;
      n = std::min(n, len);
    }
    s.buf.resize(n * nch);
    for (size_t i = 0; i < n; i++)
      for (size_t k = 0; k < nch; k++) {
        float v = src[k][i] * 32768.f;
        s.buf[i * nch + k] = (int16_t)(v > 32767.f ? 32767 : v < -32768.f ? -32768 : std::lrint(v));
      }
    P err = nullptr;
    sp_submit(s.sp, s.buf.data(), s.buf.size() * sizeof(int16_t), &err);
    if (!sp_check(err, "spotter_submit_raw_data")) return hits;
    for (int guard = 0; guard < 16; guard++) {
      int ev = sp_next(s.sp, &err);
      if (!sp_check(err, "spotter_get_next_event") || ev < 0 || ev > 9 || (kEventsStopMask >> ev & 1)) break;
      if (ev != EV_CONFIRM_1ST && ev != EV_CONFIRM_2ND) continue;
      const char *ph = sp_phrase(s.sp, &err);
      sp_check(err, "spotter_get_current_phrase");
      int64_t now = Loop::now_ms();
      if (now - s.last_hit_ms < 1200) continue;  // one hit per utterance
      s.last_hit_ms = now;
      hits.emplace_back(ph ? ph : "");
      LOGI("%s detected: '%s' (event %d)", s.what, hits.back().c_str(), ev);
    }
    return hits;
  }

  ~Engine() {
    free_spotter(wake);
    free_spotter(cmd);
    if (vqe) vqe_free(vqe);
    if (cfg) cfg_free(cfg);
  }
};

AudioFrontend::AudioFrontend(Loop &loop, const Model &model) : loop_(loop), model_(model) {}

AudioFrontend::~AudioFrontend() { stop(); }

bool AudioFrontend::start(const std::string &model_dir, const std::string &command_model_dir) {
  eng_ = std::make_unique<Engine>();
  if (!eng_->load() || !eng_->init_vqe(model_.vqe_preset) ||
      !eng_->init_spotter(eng_->wake, model_dir, VQE_OMNI_WITH_GAIN)) {
    eng_.reset();
    return false;
  }
  if (!command_model_dir.empty() && !eng_->init_spotter(eng_->cmd, command_model_dir, VQE_OMNI))
    LOGW("command spotter unavailable; voice commands without the wake word are disabled");
  running_ = true;
  thr_ = std::thread([this] { run(); });
  return true;
}

void AudioFrontend::stop() {
  running_ = false;
  if (thr_.joinable()) thr_.join();
  eng_.reset();
}

void AudioFrontend::set_model(const std::string &model_dir) {
  std::lock_guard<std::mutex> lk(model_mu_);
  pending_model_ = model_dir;
}

// Two threads: a capture thread that only drains ALSA into a FIFO, and this processing thread
// (VQE + spotters). The Lite 2's PDM capture allows at most ~42 ms of buffer (2048 frames at
// 8 ch S16); VQE chunks occasionally take longer than the remaining slack, which overran the
// device buffer when reading and processing shared one thread.
void AudioFrontend::run() {
  Engine &e = *eng_;
  const size_t period = size_t(e.chunk);
  const char *dev = model_.capture_pcm.c_str();
  const unsigned nch = model_.capture_channels;
  const auto &mic_ch = model_.mic_channels, &spk_ch = model_.spk_channels;
  const size_t nmic = mic_ch.size(), nspk = spk_ch.size();
  const size_t fifo_max = size_t(model_.capture_rate) * nch;  // 1 s of audio

  std::mutex fifo_mu;
  std::condition_variable fifo_cv;
  std::vector<int16_t> fifo;  // interleaved frames, oldest first

  std::thread reader([&] {
    snd_pcm_t *pcm = nullptr;
    snd_pcm_uframes_t hw_period = period;
    auto open_pcm = [&]() -> bool {
      int err = snd_pcm_open(&pcm, dev, SND_PCM_STREAM_CAPTURE, 0);
      if (err < 0) {
        LOGE("snd_pcm_open %s: %s", dev, snd_strerror(err));
        return false;
      }
      snd_pcm_hw_params_t *hw;
      snd_pcm_hw_params_alloca(&hw);
      snd_pcm_hw_params_any(pcm, hw);
      snd_pcm_hw_params_set_access(pcm, hw, SND_PCM_ACCESS_RW_INTERLEAVED);
      snd_pcm_hw_params_set_format(pcm, hw, SND_PCM_FORMAT_S16_LE);
      snd_pcm_hw_params_set_channels(pcm, hw, nch);
      unsigned rate = model_.capture_rate;
      snd_pcm_hw_params_set_rate_near(pcm, hw, &rate, nullptr);
      snd_pcm_uframes_t per = period, buf = period * 8;
      snd_pcm_hw_params_set_period_size_near(pcm, hw, &per, nullptr);
      snd_pcm_hw_params_set_buffer_size_near(pcm, hw, &buf);
      if ((err = snd_pcm_hw_params(pcm, hw)) < 0) {
        LOGE("snd_pcm_hw_params: %s", snd_strerror(err));
        snd_pcm_close(pcm);
        pcm = nullptr;
        return false;
      }
      LOGI("capture %s: %u Hz, %u ch, period %lu, buffer %lu", dev, rate, nch, (unsigned long)per,
           (unsigned long)buf);
      hw_period = std::min<snd_pcm_uframes_t>(per, period);
      return true;
    };
    std::vector<int16_t> in(period * nch);
    while (running_) {
      if (!pcm && !open_pcm()) {
        std::this_thread::sleep_for(std::chrono::seconds(2));
        continue;
      }
      snd_pcm_sframes_t n = snd_pcm_readi(pcm, in.data(), hw_period);
      if (n < 0) {
        LOGW("capture: %s, recovering", snd_strerror((int)n));
        if (snd_pcm_recover(pcm, (int)n, 1) < 0) {
          snd_pcm_close(pcm);
          pcm = nullptr;
        }
        continue;
      }
      std::lock_guard<std::mutex> lk(fifo_mu);
      fifo.insert(fifo.end(), in.begin(), in.begin() + n * nch);
      if (fifo.size() > fifo_max) {
        LOGW("capture: processing behind, dropping %zu frames", (fifo.size() - fifo_max) / nch);
        fifo.erase(fifo.begin(), fifo.end() - fifo_max);
      }
      fifo_cv.notify_one();
    }
    if (pcm) snd_pcm_close(pcm);
  });

  std::vector<int16_t> in(period * nch);
  std::vector<float> mics(period * nmic), spk(period * nspk);

  while (running_) {
    {
      std::lock_guard<std::mutex> lk(model_mu_);
      if (!pending_model_.empty()) {
        if (!e.init_spotter(e.wake, pending_model_, VQE_OMNI_WITH_GAIN))
          LOGE("failed to switch wake word model to %s", pending_model_.c_str());
        pending_model_.clear();
      }
    }
    // Take exactly one VQE chunk.
    {
      std::unique_lock<std::mutex> lk(fifo_mu);
      if (!fifo_cv.wait_for(lk, std::chrono::milliseconds(500), [&] { return fifo.size() >= in.size(); }))
        continue;
      std::copy(fifo.begin(), fifo.begin() + in.size(), in.begin());
      fifo.erase(fifo.begin(), fifo.begin() + in.size());
    }

    for (size_t i = 0; i < period; i++) {
      const int16_t *f = &in[i * nch];
      for (size_t c = 0; c < nmic; c++) mics[i * nmic + c] = f[mic_ch[c]] * (1.0f / 32768.f);
      for (size_t c = 0; c < nspk; c++) spk[i * nspk + c] = f[spk_ch[c]] * (1.0f / 32768.f);
    }
    if (!e.vqe_check(e.vqe_process(e.vqe, mics.data(), mics.size(), spk.data(), spk.size()), "VQE process")) continue;

    if (stream_.load() && audio_cb_) {
      const float *d = nullptr;
      size_t n = 0;
      if (e.vqe_check(e.vqe_out(e.vqe, VQE_OMNI_WITH_GAIN, 0, &d, &n), "VQE output") && n) {
        auto chunk = std::make_shared<std::vector<int16_t>>(n);
        for (size_t i = 0; i < n; i++) {
          float v = d[i] * 32768.f;
          (*chunk)[i] = (int16_t)(v > 32767.f ? 32767 : v < -32768.f ? -32768 : std::lrint(v));
        }
        loop_.post([this, chunk] {
          if (audio_cb_) audio_cb_(chunk->data(), chunk->size());
        });
      }
    }

    if (detect_.load()) {
      for (auto &phrase : e.feed(e.wake))
        loop_.post([this, phrase] {
          if (wake_cb_) wake_cb_(phrase);
        });
    }
    if (commands_.load()) {
      for (auto &phrase : e.feed(e.cmd))
        loop_.post([this, phrase] {
          if (command_cb_) command_cb_(phrase);
        });
    }
  }
  reader.join();
}

}  // namespace cs
