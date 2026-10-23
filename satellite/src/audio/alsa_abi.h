#pragma once

#include <alsa/asoundlib.h>
#include <dlfcn.h>

namespace cs::alsa {

// Midi firmware images with a 32-bit userland ship a versioned libasound (ALSA_0.9 +
// ALSA_0.9.0rc4). The armhf build links the Lite 2's unversioned one, so its references bind to
// the ALSA_0.9 compat entries of these functions, which take values instead of pointers (period /
// buffer read back as 0, wake word dead). The __snd_* aliases are the current ABI; a library
// without them has only that ABI under the plain name. Every snd_pcm_hw_params_get_* /
// set_*_near call goes through here.
template <typename Function>
Function modern_symbol(const char *alias, Function fallback) {
  auto symbol = dlsym(RTLD_DEFAULT, alias);
  return symbol ? reinterpret_cast<Function>(symbol) : fallback;
}

inline int hw_params_set_rate_near(snd_pcm_t *pcm, snd_pcm_hw_params_t *params, unsigned *rate, int *dir) {
  static auto fn = modern_symbol("__snd_pcm_hw_params_set_rate_near", &snd_pcm_hw_params_set_rate_near);
  return fn(pcm, params, rate, dir);
}
inline int hw_params_set_period_size_near(snd_pcm_t *pcm, snd_pcm_hw_params_t *params,
                                        snd_pcm_uframes_t *frames, int *dir) {
  static auto fn = modern_symbol("__snd_pcm_hw_params_set_period_size_near", &snd_pcm_hw_params_set_period_size_near);
  return fn(pcm, params, frames, dir);
}
inline int hw_params_set_buffer_size_near(snd_pcm_t *pcm, snd_pcm_hw_params_t *params, snd_pcm_uframes_t *frames) {
  static auto fn = modern_symbol("__snd_pcm_hw_params_set_buffer_size_near", &snd_pcm_hw_params_set_buffer_size_near);
  return fn(pcm, params, frames);
}
inline int hw_params_get_period_size(const snd_pcm_hw_params_t *params, snd_pcm_uframes_t *frames, int *dir) {
  static auto fn = modern_symbol("__snd_pcm_hw_params_get_period_size", &snd_pcm_hw_params_get_period_size);
  return fn(params, frames, dir);
}
inline int hw_params_get_buffer_size(const snd_pcm_hw_params_t *params, snd_pcm_uframes_t *frames) {
  static auto fn = modern_symbol("__snd_pcm_hw_params_get_buffer_size", &snd_pcm_hw_params_get_buffer_size);
  return fn(params, frames);
}

}  // namespace cs::alsa
