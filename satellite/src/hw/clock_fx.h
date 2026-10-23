// Pure rendering of the 7-segment clock animations: the stock voice / ringing / player / boot
// patterns and the music level meter. The stock envelopes were measured from the Lite 2's
// /system/vendor/quasar/ledpatterns/clock/*.binscled.zst (the Midi ships only ringing, the same);
// nothing of those files is used at run time.
#pragma once

#include <array>
#include <cstdint>

namespace cs::clockfx {

// Brightness 0..255: seg[digit][segment], digits 0..3 left to right, segments a..g = 0..6; colon =
// both colon dots; center = the dot between them at the height of g (Lite 2 "DM").
struct Frame {
  uint8_t seg[4][7] = {};
  uint8_t colon = 0;
  uint8_t center = 0;
};

// The first four loop, the others play once.
enum class Anim : uint8_t { LISTENING, THINKING, SPEAKING, RINGING, BOOT, PAUSE, NEXT, PREVIOUS };

bool loops(Anim a);
int64_t duration_ms(Anim a);  // loop period / one-shot length
// Overwrites f; loops wrap t, one-shots draw nothing at t >= duration.
void render(Anim a, int64_t t, Frame &f);
// "Levels" music style: one meter per digit (0..1), lit d → g → a bottom up.
void levels(const std::array<float, 4> &digits, Frame &f);

}  // namespace cs::clockfx
