#pragma once

#include <cstdint>

namespace tmirror {
namespace term {

/// Display width of a code point in terminal cells: 0 for combining marks and other
/// zero-width characters, 2 for East Asian Wide/Fullwidth and emoji presentation, and
/// 1 otherwise (spec §8.1, §8.2).
///
/// C0/C1 controls report 0; the emulator never asks about them because they are
/// consumed by the parser before they reach the screen.
int CharWidth(char32_t code_point);

/// True for characters that combine with the preceding cell rather than occupying
/// one of their own (Mn/Me, ZWJ, variation selectors).
bool IsZeroWidth(char32_t code_point);

}  // namespace term
}  // namespace tmirror
