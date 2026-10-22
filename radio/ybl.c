// ybl: turns a Telink B91 application binary into the .ybl container the Yandex Station Midi's
// radio bootloader takes (the format of /system/vendor/quasar/zigbee/gateway.ybl): the image as
// linked, whose header carries its size at 0x18, followed by the zlib CRC-32 of the image, LE.
// usage: ybl <in.bin> <out.ybl>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static uint32_t crc32(const uint8_t *p, size_t n) {
  uint32_t c = 0xFFFFFFFFu;
  for (size_t i = 0; i < n; i++) {
    c ^= p[i];
    for (int k = 0; k < 8; k++) c = (c >> 1) ^ (0xEDB88320u & (0u - (c & 1)));
  }
  return ~c;
}

static uint32_t le32(const uint8_t *p) { return p[0] | p[1] << 8 | p[2] << 16 | (uint32_t)p[3] << 24; }

int main(int argc, char **argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: ybl <in.bin> <out.ybl>\n");
    return 2;
  }
  FILE *f = fopen(argv[1], "rb");
  if (!f) {
    perror(argv[1]);
    return 1;
  }
  fseek(f, 0, SEEK_END);
  long n = ftell(f);
  fseek(f, 0, SEEK_SET);
  uint8_t *b = malloc((size_t)n + 4);
  if (!b || fread(b, 1, (size_t)n, f) != (size_t)n) {
    perror(argv[1]);
    return 1;
  }
  fclose(f);
  if (n < 0x40 || memcmp(b + 0x20, "KNLT", 4) != 0) {
    fprintf(stderr, "%s: no Telink image header\n", argv[1]);
    return 1;
  }
  if (le32(b + 0x18) != (uint32_t)n) {
    fprintf(stderr, "%s: header size %u, file %ld\n", argv[1], le32(b + 0x18), n);
    return 1;
  }
  uint32_t c = crc32(b, (size_t)n);
  for (int i = 0; i < 4; i++) b[n + i] = (uint8_t)(c >> (8 * i));
  f = fopen(argv[2], "wb");
  if (!f || fwrite(b, 1, (size_t)n + 4, f) != (size_t)n + 4 || fclose(f) != 0) {
    perror(argv[2]);
    return 1;
  }
  printf("%s: %ld bytes, version 0x%08x, manufacturer 0x%04x, image type 0x%04x, crc32 0x%08x\n", argv[2], n + 4,
         le32(b + 2), b[0x12] | b[0x13] << 8, b[0x14] | b[0x15] << 8, c);
  return 0;
}
