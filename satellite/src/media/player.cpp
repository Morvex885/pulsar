// GStreamer playback via the firmware's libquasar_gstreamer.so (dlopen'ed, no headers) and the
// ALSA softvol master control.
//
// Library loading: libquasar_gstreamer.so has RUNPATH=/system/vendor/quasar/libs, but libglib.so
// has none and NEEDs libiconv.so (which lives only in that directory). We dlopen libiconv.so,
// libglib.so and libquasar_gstreamer.so by absolute path, in that order, with RTLD_GLOBAL; the
// already-loaded "libiconv.so" SONAME then satisfies glib's dependency, so LD_LIBRARY_PATH is not
// needed.
//
// Threading: a dedicated thread runs a GMainLoop on the default GMainContext; bus watches live
// there. Public methods marshal work into it with g_idle_add; callbacks go back via Loop::post.
//
// Struct layouts used without headers (aarch64, GStreamer 1.x):
//   GstMessage: GstMiniObject (64 bytes) | GstMessageType type @64 | guint64 timestamp @72 |
//               GstObject *src @80
//   GError:     GQuark domain @0 | gint code @4 | gchar *message @8
#include "media/player.h"

#include <alsa/asoundlib.h>
#include <dlfcn.h>
#include <unistd.h>

#include <atomic>
#include <cmath>
#include <cstdarg>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <thread>

#include "core/log.h"

namespace cs {

namespace {

constexpr const char *kLibDir = "/system/vendor/quasar/libs/";

using gp = void *;
using GSourceFunc = int (*)(gp);
using GstBusFunc = int (*)(gp bus, gp msg, gp user);

enum : int { GST_STATE_VOID = 0, GST_STATE_NULL = 1, GST_STATE_READY = 2, GST_STATE_PAUSED = 3, GST_STATE_PLAYING = 4 };
enum : int { GST_STATE_CHANGE_FAILURE = 0 };
enum : uint32_t {
  MSG_EOS = 1u << 0,
  MSG_ERROR = 1u << 1,
  MSG_WARNING = 1u << 2,
  MSG_BUFFERING = 1u << 5,
  MSG_STATE_CHANGED = 1u << 6,
  MSG_CLOCK_LOST = 1u << 10,
};
// GstPlayFlags: AUDIO | SOFT_VOLUME (no video/text/visualisation).
constexpr unsigned kPlayFlags = (1u << 1) | (1u << 4);

inline uint32_t msg_type(gp msg) { return *reinterpret_cast<uint32_t *>(static_cast<char *>(msg) + 64); }
inline gp msg_src(gp msg) { return *reinterpret_cast<gp *>(static_cast<char *>(msg) + 80); }
inline const char *gerror_message(gp err) {
  return err ? *reinterpret_cast<const char **>(static_cast<char *>(err) + 8) : "?";
}

constexpr const char *kCaBundle = "/etc/ssl/certs/ca-certificates.crt";

// alsa-lib diagnostics (alsasink's IEC958 probe "Unknown PCM speakers:{AES0 ...}", "Cannot read
// Channel Map ctl") are noise on this device; real failures surface as GStreamer errors.
void alsa_error(const char *file, int line, const char *func, int err, const char *fmt, ...) {
  if (log_level() > 0) return;
  char buf[256];
  va_list ap;
  va_start(ap, fmt);
  vsnprintf(buf, sizeof buf, fmt, ap);
  va_end(ap);
  LOGD("alsa: %s:%d %s%s%s", file, line, buf, err ? ": " : "", err ? snd_strerror(err) : "");
}

struct Api {
  // glib
  gp (*g_main_loop_new)(gp ctx, int running);
  void (*g_main_loop_run)(gp loop);
  void (*g_main_loop_quit)(gp loop);
  void (*g_main_loop_unref)(gp loop);
  unsigned (*g_idle_add)(GSourceFunc fn, gp data);
  unsigned (*g_timeout_add)(unsigned ms, GSourceFunc fn, gp data);
  int (*g_source_remove)(unsigned id);
  void (*g_object_set)(gp obj, const char *first, ...);
  void (*g_error_free)(gp err);
  void (*g_free)(gp p);
  // gstreamer
  int (*gst_init_check)(int *argc, char ***argv, gp *err);
  gp (*gst_parse_launch)(const char *desc, gp *err);
  int (*gst_element_set_state)(gp el, int state);
  gp (*gst_pipeline_get_bus)(gp pipe);
  unsigned (*gst_bus_add_watch)(gp bus, GstBusFunc fn, gp data);
  void (*gst_message_parse_error)(gp msg, gp *err, char **dbg);
  void (*gst_message_parse_warning)(gp msg, gp *err, char **dbg);
  void (*gst_message_parse_buffering)(gp msg, int *percent);
  void (*gst_message_parse_state_changed)(gp msg, int *old_s, int *new_s, int *pending);
  void (*gst_object_unref)(gp obj);
};

Api g;  // filled once by load_api()

template <typename T>
bool sym(void *h, const char *name, T &out) {
  out = reinterpret_cast<T>(dlsym(h, name));
  if (!out) LOGE("player: missing symbol %s", name);
  return out != nullptr;
}

bool load_api(void *&glib, void *&gst) {
  std::string dir = kLibDir;
  // Preload libiconv so libglib.so's DT_NEEDED "libiconv.so" resolves without LD_LIBRARY_PATH.
  if (!dlopen((dir + "libiconv.so").c_str(), RTLD_NOW | RTLD_GLOBAL)) LOGW("player: dlopen libiconv: %s", dlerror());
  glib = dlopen((dir + "libglib.so").c_str(), RTLD_NOW | RTLD_GLOBAL);
  if (!glib) {
    LOGE("player: dlopen libglib.so: %s", dlerror());
    return false;
  }
  gst = dlopen((dir + "libquasar_gstreamer.so").c_str(), RTLD_NOW | RTLD_GLOBAL);
  if (!gst) {
    LOGE("player: dlopen libquasar_gstreamer.so: %s", dlerror());
    return false;
  }
  bool ok = true;
  ok &= sym(glib, "g_main_loop_new", g.g_main_loop_new);
  ok &= sym(glib, "g_main_loop_run", g.g_main_loop_run);
  ok &= sym(glib, "g_main_loop_quit", g.g_main_loop_quit);
  ok &= sym(glib, "g_main_loop_unref", g.g_main_loop_unref);
  ok &= sym(glib, "g_idle_add", g.g_idle_add);
  ok &= sym(glib, "g_timeout_add", g.g_timeout_add);
  ok &= sym(glib, "g_source_remove", g.g_source_remove);
  ok &= sym(glib, "g_object_set", g.g_object_set);
  ok &= sym(glib, "g_error_free", g.g_error_free);
  ok &= sym(glib, "g_free", g.g_free);
  ok &= sym(gst, "gst_init_check", g.gst_init_check);
  ok &= sym(gst, "gst_parse_launch", g.gst_parse_launch);
  ok &= sym(gst, "gst_element_set_state", g.gst_element_set_state);
  ok &= sym(gst, "gst_pipeline_get_bus", g.gst_pipeline_get_bus);
  ok &= sym(gst, "gst_bus_add_watch", g.gst_bus_add_watch);
  ok &= sym(gst, "gst_message_parse_error", g.gst_message_parse_error);
  ok &= sym(gst, "gst_message_parse_warning", g.gst_message_parse_warning);
  ok &= sym(gst, "gst_message_parse_buffering", g.gst_message_parse_buffering);
  ok &= sym(gst, "gst_message_parse_state_changed", g.gst_message_parse_state_changed);
  ok &= sym(gst, "gst_object_unref", g.gst_object_unref);
  return ok;
}

// Static plugins compiled into libquasar_gstreamer.so that playbin needs for our formats.
const char *const kPlugins[] = {"coreelements", "playback",    "typefindfunctions", "audioconvert", "audioresample",
                                "audiorate",    "volume",      "alsa_nocapturelog", "wavparse",     "ogg",
                                "opus",         "opusparse",   "flac",              "audioparsers", "libav",
                                "soup",         "isomp4",      "id3demux",          "app",          "gio",
                                "autodetect",   "pbtypes",     "hls",               "mpegtsdemux"};

std::string to_uri(const std::string &u) {
  if (u.find("://") != std::string::npos) return u;
  if (!u.empty() && u[0] == '/') return "file://" + u;
  return u;
}

const char *state_name(PlayState s) {
  switch (s) {
    case PlayState::IDLE: return "IDLE";
    case PlayState::LOADING: return "LOADING";
    case PlayState::PLAYING: return "PLAYING";
    case PlayState::PAUSED: return "PAUSED";
    case PlayState::ERROR: return "ERROR";
  }
  return "?";
}

}  // namespace

struct Player::Impl {
  struct Chan {
    Impl *impl = nullptr;
    Channel id = Channel::MUSIC;
    gp pipe = nullptr;
    // gst-thread state
    std::vector<std::string> urls;
    size_t index = 0;
    DoneCb done;
    bool active = false;     // a url list is loaded
    bool want_play = false;  // user wants PLAYING (false => paused)
    bool buffering = false;
    bool reached_playing = false;
    bool last_ok = true;
    double gain_cur = 1.0, gain_target = 1.0;
    unsigned ramp_src = 0;
    std::atomic<PlayState> state{PlayState::IDLE};
  };

  explicit Impl(Loop &l) : loop(l) {}

  Loop &loop;
  void *glib = nullptr, *gst = nullptr;
  gp mainloop = nullptr;
  std::thread thread;
  bool ok = false;
  Chan chans[2];
  std::mutex cb_mu;
  StateCb state_cb;

  // ---- marshalling into the GLib thread ----
  struct Call {
    std::function<void()> fn;
  };
  static int run_call(gp data) {
    auto *c = static_cast<Call *>(data);
    c->fn();
    delete c;
    return 0;  // G_SOURCE_REMOVE
  }
  void invoke(std::function<void()> fn) {
    if (!ok) return;
    g.g_idle_add(run_call, new Call{std::move(fn)});
  }

  // ---- gst-thread helpers ----
  void set_state(Chan &c, PlayState s) {
    PlayState prev = c.state.exchange(s);
    if (prev == s) return;
    LOGD("player: ch%d %s -> %s", int(c.id), state_name(prev), state_name(s));
    Channel ch = c.id;
    loop.post([this, ch, s] {
      StateCb cb;
      {
        std::lock_guard<std::mutex> lk(cb_mu);
        cb = state_cb;
      }
      if (cb) cb(ch, s);
    });
  }

  void finish(Chan &c, bool ok_) {
    g.gst_element_set_state(c.pipe, GST_STATE_NULL);
    c.active = false;
    c.buffering = false;
    c.urls.clear();
    DoneCb done = std::move(c.done);
    c.done = nullptr;
    if (!ok_) set_state(c, PlayState::ERROR);
    set_state(c, PlayState::IDLE);
    if (done) loop.post([done, ok_] { done(ok_); });
  }

  void start_current(Chan &c) {
    std::string uri = to_uri(c.urls[c.index]);
    LOGI("player: ch%d play [%zu/%zu] %s", int(c.id), c.index + 1, c.urls.size(), uri.c_str());
    g.gst_element_set_state(c.pipe, GST_STATE_NULL);
    g.g_object_set(c.pipe, "uri", uri.c_str(), nullptr);
    c.buffering = false;
    c.reached_playing = false;
    set_state(c, c.want_play ? PlayState::LOADING : PlayState::PAUSED);
    if (g.gst_element_set_state(c.pipe, c.want_play ? GST_STATE_PLAYING : GST_STATE_PAUSED) == GST_STATE_CHANGE_FAILURE) {
      LOGE("player: ch%d set_state failed for %s", int(c.id), uri.c_str());
      advance(c, false);
    }
  }

  // Current url finished (ok) or failed; go to the next or finish the list.
  void advance(Chan &c, bool ok_) {
    c.last_ok = ok_;
    if (c.index + 1 < c.urls.size()) {
      c.index++;
      start_current(c);
    } else {
      finish(c, ok_);
    }
  }

  void apply_target_state(Chan &c) {
    if (!c.active) return;
    int st = (c.want_play && !c.buffering) ? GST_STATE_PLAYING : GST_STATE_PAUSED;
    g.gst_element_set_state(c.pipe, st);
    if (!c.want_play)
      set_state(c, PlayState::PAUSED);
    else if (c.buffering || !c.reached_playing)
      set_state(c, PlayState::LOADING);
  }

  static int on_bus(gp, gp msg, gp user) {
    auto &c = *static_cast<Chan *>(user);
    c.impl->handle_message(c, msg);
    return 1;
  }

  void handle_message(Chan &c, gp msg) {
    uint32_t t = msg_type(msg);
    if (!c.active) return;
    switch (t) {
      case MSG_EOS:
        LOGI("player: ch%d EOS", int(c.id));
        advance(c, true);
        break;
      case MSG_ERROR: {
        gp err = nullptr;
        char *dbg = nullptr;
        g.gst_message_parse_error(msg, &err, &dbg);
        LOGE("player: ch%d error: %s (%s)", int(c.id), gerror_message(err), dbg ? dbg : "");
        if (err) g.g_error_free(err);
        if (dbg) g.g_free(dbg);
        advance(c, false);
        break;
      }
      case MSG_WARNING: {
        gp err = nullptr;
        char *dbg = nullptr;
        g.gst_message_parse_warning(msg, &err, &dbg);
        LOGW("player: ch%d warning: %s", int(c.id), gerror_message(err));
        if (err) g.g_error_free(err);
        if (dbg) g.g_free(dbg);
        break;
      }
      case MSG_BUFFERING: {
        int pct = 100;
        g.gst_message_parse_buffering(msg, &pct);
        bool buf = pct < 100;
        if (buf != c.buffering) {
          LOGD("player: ch%d buffering %d%%", int(c.id), pct);
          c.buffering = buf;
          apply_target_state(c);
        }
        break;
      }
      case MSG_STATE_CHANGED: {
        if (msg_src(msg) != c.pipe) break;
        int o = 0, n = 0, p = 0;
        g.gst_message_parse_state_changed(msg, &o, &n, &p);
        if (n == GST_STATE_PLAYING) {
          c.reached_playing = true;
          if (c.want_play && !c.buffering) set_state(c, PlayState::PLAYING);
        }
        break;
      }
      case MSG_CLOCK_LOST:
        g.gst_element_set_state(c.pipe, GST_STATE_PAUSED);
        apply_target_state(c);
        break;
      default:
        break;
    }
  }

  // Linear gain ramp (0.05 per 20 ms, full scale in 400 ms) to avoid clicks when ducking.
  static int ramp_tick(gp user) {
    auto &c = *static_cast<Chan *>(user);
    constexpr double step = 0.05;
    double d = c.gain_target - c.gain_cur;
    if (std::fabs(d) <= step) {
      c.gain_cur = c.gain_target;
    } else {
      c.gain_cur += d > 0 ? step : -step;
    }
    g.g_object_set(c.pipe, "volume", c.gain_cur, nullptr);
    if (c.gain_cur == c.gain_target) {
      c.ramp_src = 0;
      return 0;
    }
    return 1;
  }

  void gst_thread() {
    g.g_main_loop_run(mainloop);
    for (auto &c : chans) {
      if (c.ramp_src) g.g_source_remove(c.ramp_src);
      g.gst_element_set_state(c.pipe, GST_STATE_NULL);
      g.gst_object_unref(c.pipe);
      c.pipe = nullptr;
    }
  }
};

Player::Player(Loop &loop) : impl_(new Impl(loop)) {}

Player::~Player() {
  if (impl_->ok) {
    gp ml = impl_->mainloop;
    impl_->invoke([ml] { g.g_main_loop_quit(ml); });
    if (impl_->thread.joinable()) impl_->thread.join();
    g.g_main_loop_unref(ml);
  }
}

bool Player::init() {
  auto &d = *impl_;
  if (d.ok) return true;
  if (!load_api(d.glib, d.gst)) return false;
  gp err = nullptr;
  if (access(kCaBundle, R_OK) == 0) setenv("SSL_CERT_FILE", kCaBundle, 0);
  // Process-global: alsa-lib messages go to the debug log (see alsa_error).
  snd_lib_error_set_handler(alsa_error);
  if (!g.gst_init_check(nullptr, nullptr, &err)) {
    LOGE("player: gst_init_check failed: %s", gerror_message(err));
    return false;
  }
  int regs = 0;
  for (const char *p : kPlugins) {
    std::string name = std::string("gst_plugin_") + p + "_register";
    auto reg = reinterpret_cast<int (*)()>(dlsym(d.gst, name.c_str()));
    if (reg) {
      reg();
      regs++;
    } else {
      LOGW("player: plugin %s not present", p);
    }
  }
  // TLS backend for souphttpsrc (https://): glib-networking's OpenSSL module is linked statically
  // into libquasar_gstreamer.so; loading it with a NULL module registers it as a static type.
  // Its default CA store is OpenSSL's compiled-in path, which does not exist on the device
  // ("Unacceptable TLS certificate"); SSL_CERT_FILE (set above) points it at the system bundle.
  if (auto tls = reinterpret_cast<void (*)(gp)>(dlsym(d.gst, "g_io_openssl_load"))) tls(nullptr);
  LOGI("player: gstreamer initialised, %d plugins registered", regs);

  for (int i = 0; i < 2; i++) {
    auto &c = d.chans[i];
    c.impl = &d;
    c.id = Channel(i);
    gp perr = nullptr;
    char desc[160];
    snprintf(desc, sizeof desc, "playbin name=ch%d audio-sink=\"alsasink device=speakers\"", i);
    c.pipe = g.gst_parse_launch(desc, &perr);
    if (!c.pipe) {
      LOGE("player: cannot create playbin: %s", gerror_message(perr));
      return false;
    }
    if (perr) {
      LOGW("player: playbin: %s", gerror_message(perr));
      g.g_error_free(perr);
    }
    g.g_object_set(c.pipe, "flags", kPlayFlags, nullptr);
    g.g_object_set(c.pipe, "volume", 1.0, nullptr);
    gp bus = g.gst_pipeline_get_bus(c.pipe);
    g.gst_bus_add_watch(bus, &Impl::on_bus, &c);
    g.gst_object_unref(bus);
  }
  d.mainloop = g.g_main_loop_new(nullptr, 0);
  d.ok = true;
  d.thread = std::thread([&d] { d.gst_thread(); });
  return true;
}

void Player::play(Channel ch, std::vector<std::string> urls, DoneCb done) {
  auto *d = impl_.get();
  if (!d->ok) {
    if (done) d->loop.post([done] { done(false); });
    return;
  }
  d->invoke([d, ch, urls = std::move(urls), done = std::move(done)]() mutable {
    auto &c = d->chans[int(ch)];
    c.done = nullptr;  // replaced playback: previous done callback is dropped
    if (urls.empty()) {
      d->finish(c, false);
      if (done) d->loop.post([done] { done(false); });
      return;
    }
    c.urls = std::move(urls);
    c.index = 0;
    c.done = std::move(done);
    c.active = true;
    c.want_play = true;
    c.last_ok = true;
    d->start_current(c);
  });
}

void Player::pause(Channel ch) {
  auto *d = impl_.get();
  d->invoke([d, ch] {
    auto &c = d->chans[int(ch)];
    if (!c.active || !c.want_play) return;
    c.want_play = false;
    d->apply_target_state(c);
  });
}

void Player::resume(Channel ch) {
  auto *d = impl_.get();
  d->invoke([d, ch] {
    auto &c = d->chans[int(ch)];
    if (!c.active || c.want_play) return;
    c.want_play = true;
    d->apply_target_state(c);
    if (c.reached_playing && !c.buffering) d->set_state(c, PlayState::PLAYING);
  });
}

void Player::stop(Channel ch) {
  auto *d = impl_.get();
  d->invoke([d, ch] {
    auto &c = d->chans[int(ch)];
    if (!c.active) return;
    LOGI("player: ch%d stop", int(ch));
    c.done = nullptr;  // not called on stop()
    d->finish(c, true);
  });
}

PlayState Player::state(Channel ch) const { return impl_->chans[int(ch)].state.load(); }

void Player::set_channel_gain(Channel ch, float gain) {
  auto *d = impl_.get();
  double gn = std::min(1.0, std::max(0.0, double(gain)));
  d->invoke([d, ch, gn] {
    auto &c = d->chans[int(ch)];
    c.gain_target = gn;
    if (!c.active) {  // nothing audible: apply at once, no ramp
      if (c.ramp_src) g.g_source_remove(c.ramp_src);
      c.ramp_src = 0;
      c.gain_cur = gn;
      g.g_object_set(c.pipe, "volume", gn, nullptr);
      return;
    }
    if (!c.ramp_src) c.ramp_src = g.g_timeout_add(20, &Impl::ramp_tick, &c);
  });
}

void Player::on_state(StateCb cb) {
  std::lock_guard<std::mutex> lk(impl_->cb_mu);
  impl_->state_cb = std::move(cb);
}

// ---------------------------------------------------------------------------------------------
// Mixer: softvol "Master Playback Volume" on card 0 (2 values, 0..255, -60 dB..0 dB linear in dB,
// TLV mute=0 i.e. raw 0 is -60 dB, not silence). The control is a user control that alsa-lib
// creates the first time the "speakers" PCM is opened, so open() instantiates it if missing.
//
// Volume mapping (same as alsamixer's "mapped volume"): v = (10^(dB/60) - 0.1) / 0.9, i.e.
// dB = 60 * log10(0.1 + 0.9 v). v=0 -> -60 dB (raw 0), v=0.5 -> -15.6 dB, v=1 -> 0 dB.

namespace {
constexpr const char *kMasterCtl = "Master Playback Volume";
constexpr double kMinDb = -60.0;

int vol_to_raw(float v) {
  if (v <= 0.0f) return 0;
  double db = 60.0 * std::log10(0.1 + 0.9 * std::min(1.0f, v));
  int raw = int(std::lround((db - kMinDb) / -kMinDb * 255.0));
  return std::max(0, std::min(255, raw));
}

float raw_to_vol(int raw) {
  double db = kMinDb + (-kMinDb) * raw / 255.0;
  return float(std::max(0.0, (std::pow(10.0, db / 60.0) - 0.1) / 0.9));
}

bool find_master(snd_ctl_t *ctl) {
  snd_ctl_elem_id_t *id;
  snd_ctl_elem_info_t *info;
  snd_ctl_elem_id_alloca(&id);
  snd_ctl_elem_info_alloca(&info);
  snd_ctl_elem_id_set_interface(id, SND_CTL_ELEM_IFACE_MIXER);
  snd_ctl_elem_id_set_name(id, kMasterCtl);
  snd_ctl_elem_info_set_id(info, id);
  return snd_ctl_elem_info(ctl, info) == 0;
}
}  // namespace

bool Mixer::open() {
  snd_ctl_t *ctl = nullptr;
  int rc = snd_ctl_open(&ctl, "hw:0", 0);
  if (rc < 0) {
    LOGE("mixer: snd_ctl_open hw:0: %s", snd_strerror(rc));
    return false;
  }
  if (!find_master(ctl)) {
    // Opening the softvol PCM once creates the control.
    snd_pcm_t *pcm = nullptr;
    if (snd_pcm_open(&pcm, "speakers", SND_PCM_STREAM_PLAYBACK, SND_PCM_NONBLOCK) == 0) snd_pcm_close(pcm);
    if (!find_master(ctl)) {
      LOGE("mixer: control '%s' not found", kMasterCtl);
      snd_ctl_close(ctl);
      return false;
    }
  }
  ctl_ = ctl;
  // Adopt the current hardware value.
  snd_ctl_elem_value_t *val;
  snd_ctl_elem_value_alloca(&val);
  snd_ctl_elem_value_set_interface(val, SND_CTL_ELEM_IFACE_MIXER);
  snd_ctl_elem_value_set_name(val, kMasterCtl);
  if (snd_ctl_elem_read(ctl, val) == 0) {
    int raw = snd_ctl_elem_value_get_integer(val, 0);
    if (raw > 0) volume_ = raw_to_vol(raw);
    LOGI("mixer: master raw=%d -> volume %.2f", raw, volume_);
  }
  return true;
}

float Mixer::volume() const { return volume_; }

void Mixer::set_volume(float v) {
  volume_ = std::min(1.0f, std::max(0.0f, v));
  apply();
}

bool Mixer::muted() const { return muted_; }

void Mixer::set_muted(bool m) {
  muted_ = m;
  apply();
}

void Mixer::apply() {
  auto *ctl = static_cast<snd_ctl_t *>(ctl_);
  if (!ctl) return;
  int raw = muted_ ? 0 : vol_to_raw(volume_);
  snd_ctl_elem_value_t *val;
  snd_ctl_elem_value_alloca(&val);
  snd_ctl_elem_value_set_interface(val, SND_CTL_ELEM_IFACE_MIXER);
  snd_ctl_elem_value_set_name(val, kMasterCtl);
  snd_ctl_elem_value_set_integer(val, 0, raw);
  snd_ctl_elem_value_set_integer(val, 1, raw);
  int rc = snd_ctl_elem_write(ctl, val);
  if (rc < 0) LOGE("mixer: write %d: %s", raw, snd_strerror(rc));
  else LOGD("mixer: volume %.2f muted=%d -> raw %d", volume_, int(muted_), raw);
}

}  // namespace cs
