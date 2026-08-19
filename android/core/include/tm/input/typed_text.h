#pragma once

#include <cstdint>
#include <string>

#include "tm/util/bytes.h"

namespace tmirror {
namespace input {

/// How one delivery of typed text divides when a modifier is latched (spec §9.1, §9.2).
///
/// A soft keyboard delivers ordinary letters as *committed text*, and committed text
/// has nowhere to put a modifier — `KeyEncoder::EncodeText` sends it verbatim, which is
/// the whole point of it. So a latched `Ctrl` can only reach the terminal by turning one
/// character into a *key event* instead. Getting that split wrong is what makes `Ctrl`
/// then `c` type a literal "c" and leave a running program with no way to be
/// interrupted from the on-screen keyboard.
struct TypedTextPlan {
  /// Text that goes first, unmodified: whatever the keyboard was already composing
  /// before the user reached for the modifier.
  std::string leading;
  /// Whether a character becomes a modified keypress at all.
  bool has_key = false;
  char32_t unicode = 0;
  std::uint8_t modifiers = 0;
  /// Anything the same delivery carried after the modified character.
  std::string trailing;
  /// Whether the latch was used up, and so should stop being shown as held.
  bool consumes_latch = false;
};

/// Divides a typed-text delivery.
///
/// `pending` is what the keyboard is currently composing and has not sent; `value` is
/// what it now says the composition (or commit) is. `latched` is the modifier the user
/// armed from the extra-key row.
///
/// Only a composition that *grew by appending* carries a new keypress. One that
/// shortened or changed is a backspace, an autocorrect or a swipe, and the modifier
/// belongs to whatever the user presses next — reading those as a keypress turns a
/// Backspace into Ctrl+G and throws away the rest of the word.
TypedTextPlan PlanTypedText(ByteView pending, ByteView value, std::uint8_t latched);

}  // namespace input
}  // namespace tmirror
