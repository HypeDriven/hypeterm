#pragma once

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

namespace tmirror {

using Bytes = std::vector<std::uint8_t>;

/// Non-owning view over bytes. Terminal payloads move through several layers before
/// they are copied into the emulator, and each hop that does not copy is a hop that
/// cannot allocate unboundedly.
class ByteView {
 public:
  ByteView() = default;
  ByteView(const std::uint8_t* data, std::size_t size) : data_(data), size_(size) {}
  explicit ByteView(const Bytes& b) : data_(b.data()), size_(b.size()) {}
  explicit ByteView(const std::string& s)
      : data_(reinterpret_cast<const std::uint8_t*>(s.data())), size_(s.size()) {}

  static ByteView FromChars(const char* s, std::size_t n) {
    return ByteView(reinterpret_cast<const std::uint8_t*>(s), n);
  }

  const std::uint8_t* data() const { return data_; }
  std::size_t size() const { return size_; }
  bool empty() const { return size_ == 0; }
  std::uint8_t operator[](std::size_t i) const { return data_[i]; }
  const std::uint8_t* begin() const { return data_; }
  const std::uint8_t* end() const { return data_ + size_; }

  ByteView subview(std::size_t offset, std::size_t count = SIZE_MAX) const {
    if (offset > size_) offset = size_;
    std::size_t n = size_ - offset;
    if (count < n) n = count;
    return ByteView(data_ + offset, n);
  }

  Bytes to_bytes() const { return Bytes(data_, data_ + size_); }
  std::string to_string() const {
    return std::string(reinterpret_cast<const char*>(data_), size_);
  }

 private:
  const std::uint8_t* data_ = nullptr;
  std::size_t size_ = 0;
};

inline Bytes BytesFromString(const std::string& s) {
  return Bytes(s.begin(), s.end());
}

inline std::string StringFromBytes(const Bytes& b) {
  return std::string(b.begin(), b.end());
}

/// Overwrite a buffer before releasing it. Used for private keys and tokens
/// (spec §12: clear replaced sensitive buffers when feasible).
void SecureZero(void* data, std::size_t size);

inline void SecureZero(Bytes& b) { SecureZero(b.data(), b.size()); }
inline void SecureZero(std::string& s) { SecureZero(&s[0], s.size()); }

}  // namespace tmirror
