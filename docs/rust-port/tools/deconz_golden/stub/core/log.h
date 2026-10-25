#pragma once
#include <cstdio>
#define WP09_LOG(L, ...) (fprintf(stderr, L " "), fprintf(stderr, __VA_ARGS__), fputc('\n', stderr))
#define LOGD(...) WP09_LOG("D", __VA_ARGS__)
#define LOGI(...) WP09_LOG("I", __VA_ARGS__)
#define LOGW(...) WP09_LOG("W", __VA_ARGS__)
#define LOGE(...) WP09_LOG("E", __VA_ARGS__)
