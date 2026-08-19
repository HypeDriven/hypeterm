#pragma once

#include <condition_variable>
#include <cstddef>
#include <deque>
#include <mutex>
#include <utility>
#include <vector>

#include "tm/util/time.h"

namespace tmirror {

enum class PushResult {
  kOk,
  kFull,     // bounded capacity reached; caller must surface this, never swallow it
  kClosed,
};

/// Bounded queue used for every hand-off between the threads in spec §6.2.
///
/// The bound is the point: output bursts must be absorbed without unbounded
/// allocation, and the spec forbids silently discarding input, resize, subscription
/// and protocol-control messages. So there is no "drop oldest" policy here at all.
/// Producers either apply backpressure (`PushBlocking`) or are told the queue is full
/// (`Push`) and report it. Coalescing happens on the consumer side via `DrainAll`,
/// which is where merging adjacent output chunks is safe.
template <typename T>
class BoundedQueue {
 public:
  /// `max_bytes` is optional; when non-zero, `Sizer` must be supplied to Push.
  explicit BoundedQueue(std::size_t max_items, std::size_t max_bytes = 0)
      : max_items_(max_items == 0 ? 1 : max_items), max_bytes_(max_bytes) {}

  PushResult Push(T value, std::size_t bytes = 0) {
    std::unique_lock<std::mutex> lock(mutex_);
    if (closed_) return PushResult::kClosed;
    if (!HasRoom(bytes)) return PushResult::kFull;
    bytes_ += bytes;
    items_.emplace_back(std::move(value), bytes);
    lock.unlock();
    not_empty_.notify_one();
    return PushResult::kOk;
  }

  /// Blocks until there is room, the queue closes, or the deadline passes.
  /// This is the backpressure path: the socket reader stops reading rather than
  /// buffering without bound.
  PushResult PushBlocking(T value, std::size_t bytes = 0, Millis timeout_ms = -1) {
    std::unique_lock<std::mutex> lock(mutex_);
    if (timeout_ms < 0) {
      not_full_.wait(lock, [&] { return closed_ || HasRoom(bytes); });
    } else {
      not_full_.wait_for(lock, std::chrono::milliseconds(timeout_ms),
                         [&] { return closed_ || HasRoom(bytes); });
    }
    if (closed_) return PushResult::kClosed;
    if (!HasRoom(bytes)) return PushResult::kFull;
    bytes_ += bytes;
    items_.emplace_back(std::move(value), bytes);
    lock.unlock();
    not_empty_.notify_one();
    return PushResult::kOk;
  }

  bool Pop(T* out, Millis timeout_ms = -1) {
    std::unique_lock<std::mutex> lock(mutex_);
    if (timeout_ms < 0) {
      not_empty_.wait(lock, [&] { return closed_ || !items_.empty(); });
    } else {
      not_empty_.wait_for(lock, std::chrono::milliseconds(timeout_ms),
                          [&] { return closed_ || !items_.empty(); });
    }
    if (items_.empty()) return false;
    *out = std::move(items_.front().first);
    bytes_ -= items_.front().second;
    items_.pop_front();
    lock.unlock();
    not_full_.notify_all();
    return true;
  }

  bool TryPop(T* out) {
    std::unique_lock<std::mutex> lock(mutex_);
    if (items_.empty()) return false;
    *out = std::move(items_.front().first);
    bytes_ -= items_.front().second;
    items_.pop_front();
    lock.unlock();
    not_full_.notify_all();
    return true;
  }

  /// Take everything currently queued. The consumer may then coalesce.
  std::vector<T> DrainAll() {
    std::unique_lock<std::mutex> lock(mutex_);
    std::vector<T> out;
    out.reserve(items_.size());
    for (auto& item : items_) out.push_back(std::move(item.first));
    items_.clear();
    bytes_ = 0;
    lock.unlock();
    not_full_.notify_all();
    return out;
  }

  void Close() {
    {
      std::lock_guard<std::mutex> lock(mutex_);
      closed_ = true;
    }
    not_empty_.notify_all();
    not_full_.notify_all();
  }

  void Reopen() {
    std::lock_guard<std::mutex> lock(mutex_);
    closed_ = false;
  }

  bool closed() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return closed_;
  }
  std::size_t size() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return items_.size();
  }
  std::size_t bytes() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return bytes_;
  }
  bool empty() const { return size() == 0; }

 private:
  bool HasRoom(std::size_t bytes) const {
    if (items_.size() >= max_items_) return false;
    if (max_bytes_ != 0 && bytes_ + bytes > max_bytes_) {
      // An item larger than the whole bound is still accepted into an empty queue:
      // otherwise a single large paste would deadlock its producer forever.
      return items_.empty();
    }
    return true;
  }

  mutable std::mutex mutex_;
  std::condition_variable not_empty_;
  std::condition_variable not_full_;
  std::deque<std::pair<T, std::size_t>> items_;
  std::size_t max_items_;
  std::size_t max_bytes_;
  std::size_t bytes_ = 0;
  bool closed_ = false;
};

}  // namespace tmirror
