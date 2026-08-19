#include "tm/app/config.h"

#include "tm/net/url.h"

namespace tmirror {
namespace app {

Status AppConfig::Validate() const {
  Result<net::Url> url = net::ParseUrl(server_url);
  if (!url.ok()) return url.status();
  if (fallback_columns < 1 || fallback_columns > 10000) {
    return Status::Error(ErrorKind::kInvalidArgument, "fallback_columns is out of range");
  }
  if (fallback_rows < 1 || fallback_rows > 10000) {
    return Status::Error(ErrorKind::kInvalidArgument, "fallback_rows is out of range");
  }
  if (scrollback.max_lines == 0 || scrollback.max_bytes == 0) {
    return Status::Error(ErrorKind::kInvalidArgument, "scrollback limits must be non-zero");
  }
  if (command_queue_depth == 0 || pending_input_bytes == 0) {
    return Status::Error(ErrorKind::kInvalidArgument, "queue bounds must be non-zero");
  }
  if (paste_chunk_bytes == 0 || paste_max_bytes < paste_chunk_bytes) {
    return Status::Error(ErrorKind::kInvalidArgument, "paste bounds are inconsistent");
  }
  return Status::Ok();
}

}  // namespace app
}  // namespace tmirror
