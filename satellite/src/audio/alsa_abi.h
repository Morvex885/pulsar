#pragma once

#include <alsa/asoundlib.h>
#include <dlfcn.h>

namespace cs::alsa {

// The armhf build links unversioned ALSA for the Lite 2. On versioned Midi firmware,
// those relocations can bind ALSA_0.9 compatibility entry points with older signatures
// (values instead of pointers). The __ aliases name the modern ABI unambiguously.
// Unversioned firmware does not export them: keep using its normal API there.
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
