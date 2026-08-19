#include "tm/input/typed_text.h"

#include "tm/term/utf8.h"

namespace tmirror {
namespace input {

TypedTextPlan PlanTypedText(ByteView pending, ByteView value, std::uint8_t latched) {
  const std::string composing(reinterpret_cast<const char*>(pending.data()), pending.size());
  const std::string text(reinterpret_cast<const char*>(value.data()), value.size());

  TypedTextPlan plan;
  if (text.empty()) return plan;

  // No modifier armed: the text is ordinary text, whatever shape the update has.
  if (latched == 0) {
    plan.trailing = text;
    return plan;
  }

  // UTF-8 is prefix-safe, so a byte-wise prefix is a character-wise one and this cannot
  // split a character in half.
  const bool grew = text.size() > composing.size() && text.compare(0, composing.size(), composing) == 0;
  if (!grew) {
    // An edit, not a keypress. The latch stays armed for whatever comes next, and the
    // caller keeps tracking the composition as it normally would.
    return plan;
  }

  const std::string added = text.substr(composing.size());
  const std::u32string decoded = term::DecodeUtf8Lossy(ByteView(added));
  if (decoded.empty()) return plan;

  plan.leading = composing;
  plan.has_key = true;
  plan.unicode = decoded.front();
  plan.modifiers = latched;
  plan.consumes_latch = true;
  // The latch covers exactly one character, as it does for a hardware key; anything the
  // same delivery carried after it is ordinary text.
  plan.trailing = term::EncodeUtf8(decoded.substr(1));
  return plan;
}

}  // namespace input
}  // namespace tmirror
