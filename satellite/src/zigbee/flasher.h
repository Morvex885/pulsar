// Writes an application image into the Midi's Zigbee radio through its resident bootloader
// ("Yandex Bootloader v1.0.02": boot line held while reset is pulsed, then a text menu on the same
// UART at 115200: "1. upload ybl", "2. run", prompt "BL > "; the image goes over XMODEM, 128-byte
// blocks, CRC-16 or checksum as the receiver asks). This is maind's own update path
// (ncp_manager.cpp updateFirmware); the bootloader itself (flash 0..0x7FFF) is never written.
#pragma once

#include <cstdint>
#include <functional>
#include <string>
#include <vector>

namespace cs {

// Header of a .ybl (Telink image + zlib CRC-32 trailer, see radio/ybl.c).
struct YblInfo {
  uint32_t version = 0;
  uint16_t manufacturer = 0;
  uint16_t image_type = 0;
};

// Checks a .ybl the way maind does before uploading (size field, Telink start flag, CRC-32,
// manufacturer 0x140A, image type 0x0580).
bool ybl_parse(const std::vector<uint8_t> &file, YblInfo *info, std::string *error);
bool ybl_load(const std::string &path, std::vector<uint8_t> *file, YblInfo *info, std::string *error);

struct RadioLines {
  std::string tty, reset, boot;  // UART and the zb_control reset / boot value files
};

// Blocking (tens of seconds): run it off the loop thread, with nothing else holding the tty.
// progress gets 0..1. Leaves the radio running its application (menu item 2) whether or not the
// upload succeeded.
bool radio_flash(const RadioLines &lines, const std::vector<uint8_t> &image, const std::function<void(float)> &progress,
                 std::string *error);

}  // namespace cs
