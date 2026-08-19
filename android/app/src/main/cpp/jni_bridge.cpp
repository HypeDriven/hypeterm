// JNI bridge (spec §6.1).
//
// This file is the whole JVM boundary. It owns no terminal, protocol or rendering
// logic: it constructs the native controller and render thread, forwards platform
// events into them, and marshals their callbacks back. Anything more than that
// belongs in core/.

#include <android/log.h>
#include <unistd.h>

#include <thread>
#include <android/native_window_jni.h>
#include <jni.h>

#include <memory>
#include <string>
#include <vector>

#include "android_rasterizer.h"
#include "render_thread.h"
#include "tm/app/controller.h"
#include "tm/net/tailscale_dialer.h"
#include "tm/app/persistence.h"
#include "tm/app/session.h"
#include "tm/input/key_encoder.h"
#include "tm/input/typed_text.h"
#include "tm/util/json.h"
#include "tm/term/utf8.h"
#include "tm/util/log.h"

namespace {

constexpr const char kTag[] = "Hypeterm";

using tmirror::ByteView;
using tmirror::Bytes;
using tmirror::ErrorKind;
using tmirror::Json;
using tmirror::Result;
using tmirror::Status;
using tmirror::android::AndroidRasterizer;
using tmirror::android::RenderThread;
using tmirror::api::SecureStore;
using tmirror::app::AppConfig;
using tmirror::app::ConnectionStateName;
using tmirror::app::Controller;
using tmirror::app::ControllerCallbacks;
using tmirror::app::PairingInfo;
using tmirror::app::Preferences;
using tmirror::app::SessionStatus;

/// Repairs a string on its way to the JVM.
///
/// `NewStringUTF` takes its bytes on trust and reads past a truncated sequence, and
/// several of the strings that reach it — the window title, a clipboard write, a
/// relay-supplied label, a message quoting either — are ultimately whatever the far end
/// sent. Everything off the wire is hostile until proven otherwise (spec §12).
std::string ToJvmText(const std::string& value) {
  return tmirror::term::EncodeUtf8(tmirror::term::DecodeUtf8Lossy(ByteView(value)));
}

/// Every string handed back to the JVM goes through here, for the reason in ToJvmText.
/// One chokepoint, so a JNI method added later cannot quietly skip it.
jstring NewJvmString(JNIEnv* env, const std::string& value) {
  return env->NewStringUTF(ToJvmText(value).c_str());
}

std::string ToStdString(JNIEnv* env, jstring value) {
  if (value == nullptr) return std::string();
  const char* chars = env->GetStringUTFChars(value, nullptr);
  if (chars == nullptr) return std::string();
  std::string out(chars);
  env->ReleaseStringUTFChars(value, chars);
  return out;
}

/// Attaches the calling thread when needed; native threads start detached.
class ScopedEnv {
 public:
  explicit ScopedEnv(JavaVM* vm) : vm_(vm) {
    jint result = vm_->GetEnv(reinterpret_cast<void**>(&env_), JNI_VERSION_1_6);
    if (result == JNI_EDETACHED && vm_->AttachCurrentThread(&env_, nullptr) == JNI_OK) {
      attached_ = true;
    } else if (result != JNI_OK) {
      env_ = nullptr;
    }
  }
  ~ScopedEnv() {
    if (attached_) vm_->DetachCurrentThread();
  }
  JNIEnv* get() const { return env_; }
  explicit operator bool() const { return env_ != nullptr; }

 private:
  JavaVM* vm_;
  JNIEnv* env_ = nullptr;
  bool attached_ = false;
};

/// Keystore-backed storage, implemented on the JVM side (spec §12). The core never
/// learns how the sealing works, only that values put here are protected.
class JvmSecureStore : public SecureStore {
 public:
  JvmSecureStore(JavaVM* vm, jobject store) : vm_(vm) {
    ScopedEnv env(vm_);
    if (!env) return;
    store_ = env.get()->NewGlobalRef(store);
    jclass clazz = env.get()->GetObjectClass(store_);
    put_ = env.get()->GetMethodID(clazz, "put", "(Ljava/lang/String;[B)Z");
    get_ = env.get()->GetMethodID(clazz, "get", "(Ljava/lang/String;)[B");
    remove_ = env.get()->GetMethodID(clazz, "remove", "(Ljava/lang/String;)Z");
    contains_ = env.get()->GetMethodID(clazz, "contains", "(Ljava/lang/String;)Z");
    env.get()->DeleteLocalRef(clazz);
  }

  ~JvmSecureStore() override {
    ScopedEnv env(vm_);
    if (env && store_ != nullptr) env.get()->DeleteGlobalRef(store_);
  }

  Status Put(const std::string& key, ByteView value) override {
    ScopedEnv env(vm_);
    if (!env || store_ == nullptr) return Fail();
    jstring name = env.get()->NewStringUTF(key.c_str());
    jbyteArray array = env.get()->NewByteArray(static_cast<jsize>(value.size()));
    env.get()->SetByteArrayRegion(array, 0, static_cast<jsize>(value.size()),
                                  reinterpret_cast<const jbyte*>(value.data()));
    jboolean ok = env.get()->CallBooleanMethod(store_, put_, name, array);
    env.get()->DeleteLocalRef(array);
    env.get()->DeleteLocalRef(name);
    return ok == JNI_TRUE ? Status::Ok() : Fail();
  }

  Result<Bytes> Get(const std::string& key) override {
    ScopedEnv env(vm_);
    if (!env || store_ == nullptr) return Fail();
    jstring name = env.get()->NewStringUTF(key.c_str());
    jbyteArray array =
        static_cast<jbyteArray>(env.get()->CallObjectMethod(store_, get_, name));
    env.get()->DeleteLocalRef(name);
    if (array == nullptr) {
      return Status::Error(ErrorKind::kNotFound, "no stored value");
    }
    jsize length = env.get()->GetArrayLength(array);
    Bytes out(static_cast<std::size_t>(length));
    env.get()->GetByteArrayRegion(array, 0, length, reinterpret_cast<jbyte*>(out.data()));
    env.get()->DeleteLocalRef(array);
    return out;
  }

  Status Remove(const std::string& key) override {
    ScopedEnv env(vm_);
    if (!env || store_ == nullptr) return Fail();
    jstring name = env.get()->NewStringUTF(key.c_str());
    env.get()->CallBooleanMethod(store_, remove_, name);
    env.get()->DeleteLocalRef(name);
    return Status::Ok();
  }

  bool Contains(const std::string& key) override {
    ScopedEnv env(vm_);
    if (!env || store_ == nullptr) return false;
    jstring name = env.get()->NewStringUTF(key.c_str());
    jboolean present = env.get()->CallBooleanMethod(store_, contains_, name);
    env.get()->DeleteLocalRef(name);
    return present == JNI_TRUE;
  }

 private:
  static Status Fail() {
    return Status::Error(ErrorKind::kStorageError, "secure storage is unavailable");
  }

  JavaVM* vm_;
  jobject store_ = nullptr;
  jmethodID put_ = nullptr;
  jmethodID get_ = nullptr;
  jmethodID remove_ = nullptr;
  jmethodID contains_ = nullptr;
};

std::string StatusToJson(const SessionStatus& status) {
  Json object = Json::Object();
  object.Set("state", Json::String(ConnectionStateName(status.state)));
  object.Set("input_available", Json::Bool(status.input_available));
  object.Set("network_available", Json::Bool(status.network_available));
  object.Set("terminal_id", Json::String(status.terminal_id));
  object.Set("terminal_label", Json::String(status.terminal_label));
  object.Set("columns", Json::Int(status.columns));
  object.Set("rows", Json::Int(status.rows));
  object.Set("next_offset", Json::Uint(status.next_offset));
  object.Set("durable_offset", Json::Uint(status.durable_offset));
  object.Set("unacknowledged_input_bytes", Json::Uint(status.unacknowledged_input_bytes));
  object.Set("error_kind", Json::String(tmirror::ErrorKindName(status.last_error.kind())));
  object.Set("error_message", Json::String(status.last_error.message()));
  return object.Serialize();
}

std::string TerminalsToJson(const std::vector<tmirror::api::TerminalInfo>& terminals) {
  Json array = Json::Array();
  for (const auto& terminal : terminals) {
    Json item = Json::Object();
    item.Set("terminal_id", Json::String(terminal.terminal_id));
    item.Set("label", Json::String(terminal.label));
    item.Set("state", Json::String(terminal.state));
    item.Set("columns", Json::Uint(terminal.columns));
    item.Set("rows", Json::Uint(terminal.rows));
    item.Set("accepts_input", Json::Bool(terminal.accepts_input));
    item.Set("last_activity_at", Json::String(terminal.last_activity_at));
    item.Set("retained_bytes", Json::Uint(terminal.retained_bytes));
    array.Append(std::move(item));
  }
  Json root = Json::Object();
  root.Set("terminals", std::move(array));
  return root.Serialize();
}

/// Sends anything written to stderr to the Android log, in debug builds only.
///
/// The Go runtime reports a panic by writing to file descriptor 2, and Android throws
/// that away — an abort inside the embedded node arrives as a bare SIGABRT with no
/// message. This makes it readable.
///
/// Compiled out of release builds: spec §9.3 and §15 allow no unredacted output there,
/// and a relay of a descriptor the process does not control cannot promise redaction.
void StartStderrRelay() {
#if defined(TM_DEBUG_BUILD)
  static bool started = false;
  if (started) return;
  started = true;

  int fds[2];
  if (::pipe(fds) != 0) return;
  if (::dup2(fds[1], STDERR_FILENO) < 0) {
    ::close(fds[0]);
    ::close(fds[1]);
    return;
  }
  ::close(fds[1]);

  std::thread([read_fd = fds[0]]() {
    std::string pending;
    char buffer[512];
    ssize_t count;
    while ((count = ::read(read_fd, buffer, sizeof(buffer))) > 0) {
      pending.append(buffer, static_cast<std::size_t>(count));
      std::size_t newline;
      while ((newline = pending.find('\n')) != std::string::npos) {
        __android_log_print(ANDROID_LOG_WARN, kTag, "stderr: %s",
                            pending.substr(0, newline).c_str());
        pending.erase(0, newline + 1);
      }
      // A writer that never emits a newline must not grow the buffer without bound.
      if (pending.size() > 8192) {
        __android_log_print(ANDROID_LOG_WARN, kTag, "stderr: %s", pending.c_str());
        pending.clear();
      }
    }
    ::close(read_fd);
  }).detach();
#endif
}

/// The embedded Tailscale node's settings, parsed from the same configuration JSON
/// but kept separate because the dialer must be constructed before the controller
/// that points at it.
struct TunnelSettings {
  bool enabled = false;
  bool allow_cleartext = false;
  tmirror::net::TailscaleConfig node;
};

TunnelSettings ParseTunnel(const std::string& json) {
  TunnelSettings settings;
  Result<Json> parsed = Json::Parse(json);
  if (!parsed.ok() || !parsed.value().is_object()) return settings;
  const Json* tunnel = parsed.value().Find("tunnel");
  if (tunnel == nullptr || !tunnel->is_object()) return settings;
  settings.enabled = tunnel->GetBool("enabled", false);
  settings.allow_cleartext = tunnel->GetBool("allow_cleartext", false);
  settings.node.state_dir = tunnel->GetString("state_dir");
  settings.node.hostname = tunnel->GetString("hostname", "hypeterm");
  settings.node.control_url = tunnel->GetString("control_url");
  return settings;
}

std::string TunnelStatusToJson(const tmirror::net::TailscaleStatus& status) {
  Json object = Json::Object();
  object.Set("available", Json::Bool(status.available));
  object.Set("started", Json::Bool(status.started));
  object.Set("running", Json::Bool(status.running));
  object.Set("backend_state", Json::String(status.backend_state));
  object.Set("auth_url", Json::String(status.auth_url));
  object.Set("hostname", Json::String(status.hostname));
  object.Set("peers", Json::Int(status.peers));
  object.Set("no_log_upload", Json::Bool(status.no_log_upload));
  object.Set("cache_dir", Json::String(status.cache_dir));
  object.Set("temp_dir", Json::String(status.temp_dir));
  object.Set("last_error", Json::String(status.last_error));
  Json addresses = Json::Array();
  for (const std::string& address : status.addresses) {
    addresses.Append(Json::String(address));
  }
  object.Set("addresses", std::move(addresses));
  return object.Serialize();
}

/// Everything one attached app instance owns.
struct NativeSession {
  JavaVM* vm = nullptr;
  jobject callbacks = nullptr;
  jmethodID on_status = nullptr;
  jmethodID on_terminals = nullptr;
  jmethodID on_title = nullptr;
  jmethodID on_message = nullptr;
  jmethodID on_bell = nullptr;
  jmethodID on_clipboard = nullptr;
  jmethodID on_follow = nullptr;

  std::unique_ptr<JvmSecureStore> secure_store;
  std::unique_ptr<Preferences> preferences;
  std::unique_ptr<AndroidRasterizer> rasterizer;
  std::unique_ptr<RenderThread> render_thread;
  // Declared before the controller so it is destroyed *after* it: the controller's
  // configuration holds a bare pointer to it.
  std::unique_ptr<tmirror::net::TailscaleDialer> tunnel;
  TunnelSettings tunnel_settings;
  std::unique_ptr<Controller> controller;

  /// The chokepoint every string crosses on its way to the JVM.
  ///
  /// `NewStringUTF` takes the bytes on trust and reads past a truncated sequence, and
  /// several of these strings — the window title, a clipboard write, a relay-supplied
  /// label — are ultimately whatever the far end sent. Repairing here rather than at
  /// each caller means a new callback cannot forget to (spec §12: everything off the
  /// wire is hostile until proven otherwise).
  void CallString(jmethodID method, const std::string& value) {
    if (method == nullptr) return;
    ScopedEnv env(vm);
    if (!env) return;
    jstring text = NewJvmString(env.get(), value);
    env.get()->CallVoidMethod(callbacks, method, text);
    env.get()->DeleteLocalRef(text);
    if (env.get()->ExceptionCheck()) env.get()->ExceptionClear();
  }
};

NativeSession* FromHandle(jlong handle) {
  return reinterpret_cast<NativeSession*>(static_cast<intptr_t>(handle));
}

AppConfig ParseConfig(const std::string& json) {
  AppConfig config;
  Result<Json> parsed = Json::Parse(json);
  if (!parsed.ok() || !parsed.value().is_object()) return config;
  const Json& object = parsed.value();

  config.server_url = object.GetString("server_url", config.server_url);
  config.device_name = object.GetString("device_name", config.device_name);
  std::uint64_t number = 0;
  if (object.GetUint64("scrollback_lines", &number) && number > 0) {
    config.scrollback.max_lines = static_cast<std::size_t>(number);
  }
  if (object.GetUint64("scrollback_bytes", &number) && number > 0) {
    config.scrollback.max_bytes = static_cast<std::size_t>(number);
  }
  config.follow_remote_size = object.GetBool("follow_remote_size", true);
  config.allow_clipboard_write = object.GetBool("allow_clipboard_write", false);
  config.secure_window = object.GetBool("secure_window", false);
  config.detach_when_backgrounded = object.GetBool("detach_when_backgrounded", true);

  // Certificate trust anchors exported from the platform store, because OpenSSL
  // cannot read Android's own (see TlsConfig).
  const Json* anchors = object.Find("trust_anchors_pem");
  if (anchors != nullptr && anchors->is_array()) {
    for (const Json& anchor : anchors->items()) {
      if (anchor.is_string()) config.tls.trust_anchors_pem.push_back(anchor.string_value());
    }
    config.tls.use_default_trust_store = false;
  }
  return config;
}

}  // namespace

extern "C" {

// Static (companion @JvmStatic), so the second argument is the class, not an instance.
JNIEXPORT jlong JNICALL
Java_com_hypedriven_hypeterm_NativeBridge_nativeCreate(
    JNIEnv* env, jclass /*clazz*/, jstring config_json, jstring preferences_path,
    jobject secure_store, jobject rasterizer, jobject callbacks, jfloat font_size_px,
    jfloat density) {
  JavaVM* vm = nullptr;
  if (env->GetJavaVM(&vm) != JNI_OK) return 0;
  StartStderrRelay();

  auto session = std::make_unique<NativeSession>();
  session->vm = vm;
  session->callbacks = env->NewGlobalRef(callbacks);
  jclass callback_class = env->GetObjectClass(callbacks);
  session->on_status = env->GetMethodID(callback_class, "onStatus", "(Ljava/lang/String;)V");
  session->on_terminals =
      env->GetMethodID(callback_class, "onTerminals", "(Ljava/lang/String;)V");
  session->on_title = env->GetMethodID(callback_class, "onTitle", "(Ljava/lang/String;)V");
  session->on_message =
      env->GetMethodID(callback_class, "onMessage", "(Ljava/lang/String;Ljava/lang/String;)V");
  session->on_bell = env->GetMethodID(callback_class, "onBell", "()V");
  session->on_clipboard =
      env->GetMethodID(callback_class, "onClipboardWrite", "(Ljava/lang/String;)V");
  session->on_follow =
      env->GetMethodID(callback_class, "onFollowOutputChanged", "(Z)V");
  env->DeleteLocalRef(callback_class);

  // Android's log is the only sink available, and it never receives payload:
  // TM_LOG_PAYLOAD compiles away outside debug builds (spec §9.3, §15).
  tmirror::Log::SetSink([](tmirror::LogLevel level, const std::string& tag,
                           const std::string& message) {
    int priority = ANDROID_LOG_INFO;
    switch (level) {
      case tmirror::LogLevel::kVerbose: priority = ANDROID_LOG_VERBOSE; break;
      case tmirror::LogLevel::kDebug: priority = ANDROID_LOG_DEBUG; break;
      case tmirror::LogLevel::kInfo: priority = ANDROID_LOG_INFO; break;
      case tmirror::LogLevel::kWarn: priority = ANDROID_LOG_WARN; break;
      case tmirror::LogLevel::kError: priority = ANDROID_LOG_ERROR; break;
      case tmirror::LogLevel::kOff: return;
    }
    __android_log_print(priority, kTag, "%s: %s", tag.c_str(), message.c_str());
  });

  session->secure_store = std::make_unique<JvmSecureStore>(vm, secure_store);
  session->preferences = std::make_unique<Preferences>(ToStdString(env, preferences_path));
  session->preferences->Load();
  session->rasterizer = std::make_unique<AndroidRasterizer>(vm, rasterizer);
  session->render_thread =
      std::make_unique<RenderThread>(session->rasterizer.get(), font_size_px, density);

  const std::string config_text = ToStdString(env, config_json);
  AppConfig config = ParseConfig(config_text);

  // The tunnel is constructed before the controller, because the controller's
  // configuration holds a pointer to it. When it is off, the pointer stays null and
  // every connection goes out through the ordinary network stack.
  session->tunnel_settings = ParseTunnel(config_text);
  if (session->tunnel_settings.enabled) {
    session->tunnel =
        std::make_unique<tmirror::net::TailscaleDialer>(session->tunnel_settings.node);
    config.dialer = session->tunnel.get();
    config.allow_cleartext_over_tunnel = session->tunnel_settings.allow_cleartext;
  }

  NativeSession* raw = session.get();
  ControllerCallbacks controller_callbacks;
  controller_callbacks.on_status = [raw](const SessionStatus& status) {
    raw->CallString(raw->on_status, StatusToJson(status));
  };
  controller_callbacks.on_terminals =
      [raw](const std::vector<tmirror::api::TerminalInfo>& terminals) {
        raw->CallString(raw->on_terminals, TerminalsToJson(terminals));
      };
  controller_callbacks.on_title = [raw](const std::string& title) {
    raw->CallString(raw->on_title, title);
  };
  controller_callbacks.on_message = [raw](ErrorKind kind, const std::string& message) {
    ScopedEnv scoped(raw->vm);
    if (!scoped || raw->on_message == nullptr) return;
    jstring kind_string = scoped.get()->NewStringUTF(tmirror::ErrorKindName(kind));
    jstring text = NewJvmString(scoped.get(), message);
    scoped.get()->CallVoidMethod(raw->callbacks, raw->on_message, kind_string, text);
    scoped.get()->DeleteLocalRef(text);
    scoped.get()->DeleteLocalRef(kind_string);
    if (scoped.get()->ExceptionCheck()) scoped.get()->ExceptionClear();
  };
  controller_callbacks.on_bell = [raw]() {
    ScopedEnv scoped(raw->vm);
    if (!scoped || raw->on_bell == nullptr) return;
    scoped.get()->CallVoidMethod(raw->callbacks, raw->on_bell);
    if (scoped.get()->ExceptionCheck()) scoped.get()->ExceptionClear();
  };
  controller_callbacks.on_clipboard_write = [raw](const std::string& text) {
    raw->CallString(raw->on_clipboard, text);
  };
  // The render thread never touches the controller; it only receives snapshots.
  controller_callbacks.on_frame = [raw](const tmirror::term::SnapshotRef& snapshot) {
    raw->render_thread->SetSnapshot(snapshot);
  };

  session->controller = std::make_unique<Controller>(
      config, session->secure_store.get(), session->preferences.get(), controller_callbacks);

  // A grid change is published to the relay as a *request* (spec §10.3).
  session->render_thread->SetGridCallback([raw](int columns, int rows) {
    if (raw->controller) raw->controller->SetGridSize(columns, rows);
  });

  // Whether the view is still following the newest output, so the screen can offer a
  // way back to it. Edge-triggered on the render thread, so this is a handful of calls
  // per session rather than one per frame.
  session->render_thread->SetFollowCallback([raw](bool following) {
    ScopedEnv scoped(raw->vm);
    if (!scoped || raw->on_follow == nullptr) return;
    scoped.get()->CallVoidMethod(raw->callbacks, raw->on_follow,
                                 following ? JNI_TRUE : JNI_FALSE);
    if (scoped.get()->ExceptionCheck()) scoped.get()->ExceptionClear();
  });
  session->render_thread->Start();

  return static_cast<jlong>(reinterpret_cast<intptr_t>(session.release()));
}

JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeStart(
    JNIEnv* env, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("no session");
  Status status = session->controller->Start();
  return status.ok() ? env->NewStringUTF("") : NewJvmString(env, status.ToString());
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeDestroy(
    JNIEnv* env, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return;
  session->controller->Stop();
  session->render_thread->Stop();
  session->preferences->Save();
  if (session->callbacks != nullptr) env->DeleteGlobalRef(session->callbacks);
  delete session;
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSetSurface(
    JNIEnv* env, jobject, jlong handle, jobject surface, jint width, jint height) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return;
  ANativeWindow* window = surface != nullptr ? ANativeWindow_fromSurface(env, surface) : nullptr;
  session->render_thread->SetSurface(window, width, height);
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSetFontSize(
    JNIEnv*, jobject, jlong handle, jfloat font_size_px, jfloat density) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return;
  session->render_thread->SetFontSize(font_size_px, density);
}

JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSetServerUrl(
    JNIEnv* env, jobject, jlong handle, jstring url) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("no session");
  Status status = session->controller->SetServerUrl(ToStdString(env, url));
  return status.ok() ? env->NewStringUTF("") : NewJvmString(env, status.message());
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSetColors(
    JNIEnv*, jobject, jlong handle, jint foreground_argb, jint background_argb,
    jfloat minimum_contrast) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return;
  auto unpack = [](jint value) {
    tmirror::render::Rgba color;
    color.r = static_cast<std::uint8_t>((value >> 16) & 0xFF);
    color.g = static_cast<std::uint8_t>((value >> 8) & 0xFF);
    color.b = static_cast<std::uint8_t>(value & 0xFF);
    color.a = 255;
    return color;
  };
  session->render_thread->SetColors(unpack(foreground_argb), unpack(background_argb),
                                    minimum_contrast);
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeRefreshTerminals(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->controller->RefreshTerminals();
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeAttach(
    JNIEnv* env, jobject, jlong handle, jstring terminal_id) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->controller->Attach(ToStdString(env, terminal_id));
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeDetach(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->controller->Detach();
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSendKey(
    JNIEnv*, jobject, jlong handle, jint key, jint unicode, jint modifiers, jboolean repeat) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return;
  tmirror::input::KeyEvent event;
  event.key = static_cast<tmirror::input::Key>(key);
  event.unicode = static_cast<char32_t>(unicode);
  event.modifiers = static_cast<std::uint8_t>(modifiers);
  event.repeat = repeat == JNI_TRUE;
  // Whether a modifier latched in the extra-key row actually arrives with the character
  // the keyboard delivered is the one link no test on this machine can reach: it runs
  // through the IME, which needs a real keyboard on a real device. So it says so itself.
  // Metadata only — the character never reaches a log (spec §9.3, §15).
  TM_LOG_DEBUG(kTag, "key %d unicode:%s modifiers 0x%02x", key,
               unicode != 0 ? "yes" : "no", static_cast<unsigned>(modifiers));
  session->controller->SendKey(event);
}

/// One delivery of typed text, with whatever modifier the extra-key row has latched.
///
/// The decision about how that divides is `input::PlanTypedText` in core, not code
/// here and not code in Kotlin: it is terminal input logic, and it is the one piece of
/// this app that a keyboard can get wrong in a way no test on a developer machine would
/// see. Returns true when the latch was spent, which is all the JVM side needs to know.
JNIEXPORT jboolean JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSendTypedText(
    JNIEnv* env, jobject, jlong handle, jstring pending, jstring value, jint modifiers) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return JNI_FALSE;
  const std::string composing = ToStdString(env, pending);
  const std::string text = ToStdString(env, value);
  const tmirror::input::TypedTextPlan plan = tmirror::input::PlanTypedText(
      ByteView(composing), ByteView(text), static_cast<std::uint8_t>(modifiers));

  TM_LOG_DEBUG(kTag, "typed pending:%zu value:%zu modifiers 0x%02x -> key:%s", composing.size(),
               text.size(), static_cast<unsigned>(modifiers), plan.has_key ? "yes" : "no");

  // In order, on one queue: anything typed before the modifier reaches the shell ahead
  // of the key it precedes.
  if (!plan.leading.empty()) session->controller->SendText(plan.leading);
  if (plan.has_key) {
    tmirror::input::KeyEvent event;
    event.unicode = plan.unicode;
    event.modifiers = plan.modifiers;
    session->controller->SendKey(event);
  }
  if (!plan.trailing.empty()) session->controller->SendText(plan.trailing);
  return plan.consumes_latch ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSendText(
    JNIEnv* env, jobject, jlong handle, jstring text) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return;
  const std::string value = ToStdString(env, text);
  // The counterpart to the log above, and the one that distinguishes the two routes a
  // typed character can take: committed text carries no modifiers, so a character
  // appearing here while a modifier is latched is the bug, not the fix. Byte count only.
  TM_LOG_DEBUG(kTag, "text %zu bytes", value.size());
  session->controller->SendText(value);
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativePaste(
    JNIEnv* env, jobject, jlong handle, jstring text) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->controller->Paste(ToStdString(env, text));
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSendMouse(
    JNIEnv*, jobject, jlong handle, jint button, jint action, jint column, jint row,
    jint modifiers) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return;
  tmirror::input::MouseEvent event;
  event.button = static_cast<tmirror::input::MouseButton>(button);
  event.action = static_cast<tmirror::input::MouseAction>(action);
  event.column = column;
  event.row = row;
  event.modifiers = static_cast<std::uint8_t>(modifiers);
  session->controller->SendMouse(event);
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeScroll(
    JNIEnv*, jobject, jlong handle, jint lines) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->controller->ScrollLines(lines);
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeScrollToBottom(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->controller->ScrollToBottom();
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSetSelection(
    JNIEnv*, jobject, jlong handle, jint start_row, jint start_column, jint end_row,
    jint end_column, jboolean rectangular) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return;
  tmirror::term::Selection selection;
  selection.active = true;
  selection.start_row = start_row;
  selection.start_column = start_column;
  selection.end_row = end_row;
  selection.end_column = end_column;
  selection.rectangular = rectangular == JNI_TRUE;
  // Before the selection is queued, not after: a selection is anchored to viewport
  // rows, so a view that moves under one leaves the highlight covering different text.
  // This is the one thing that ends following without being a movement of the view
  // itself (spec §5.2).
  session->render_thread->SetFollowOutput(false);
  session->controller->SetSelection(selection);
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeClearSelection(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->controller->ClearSelection();
}

JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSelectedText(
    JNIEnv* env, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("");
  return NewJvmString(env, session->controller->SelectedText());
}

JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeVisibleText(
    JNIEnv* env, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("");
  return NewJvmString(env, session->controller->VisibleText());
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSetFocused(
    JNIEnv*, jobject, jlong handle, jboolean focused) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return;
  session->controller->SetFocused(focused == JNI_TRUE);
  session->render_thread->SetFocused(focused == JNI_TRUE);
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSetPaused(
    JNIEnv*, jobject, jlong handle, jboolean paused) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->controller->SetPaused(paused == JNI_TRUE);
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeSetNetworkAvailable(
    JNIEnv*, jobject, jlong handle, jboolean available) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->controller->SetNetworkAvailable(available == JNI_TRUE);
}

JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeStatus(
    JNIEnv* env, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("{}");
  return NewJvmString(env, StatusToJson(session->controller->status()));
}

/// Asks a machine to open a terminal (relay spec §4.6).
///
/// Blocking — it waits for the far machine to start a process. Kotlin calls it off the
/// main thread. Returns the new terminal as JSON, or an `error` object.
JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeOpenTerminal(
    JNIEnv* env, jobject, jlong handle, jstring device_id, jstring label, jint columns,
    jint rows) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("{\"error\":\"no session\"}");
  Result<tmirror::api::TerminalInfo> opened = session->controller->OpenTerminal(
      ToStdString(env, device_id), ToStdString(env, label), columns, rows);
  Json object = Json::Object();
  if (!opened.ok()) {
    object.Set("error", Json::String(opened.status().message()));
    return NewJvmString(env, object.Serialize());
  }
  object.Set("terminal_id", Json::String(opened.value().terminal_id));
  object.Set("device_id", Json::String(opened.value().device_id));
  object.Set("label", Json::String(opened.value().label));
  return NewJvmString(env, object.Serialize());
}

/// The machines this identity owns, so the user can pick one to ask. Blocking.
JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeListDevices(
    JNIEnv* env, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("{\"devices\":[]}");
  Result<std::vector<tmirror::api::DeviceInfo>> listed = session->controller->ListDevices();
  Json object = Json::Object();
  Json array = Json::Array();
  if (listed.ok()) {
    for (const auto& device : listed.value()) {
      Json item = Json::Object();
      item.Set("device_id", Json::String(device.device_id));
      item.Set("name", Json::String(device.name));
      item.Set("role", Json::String(device.role));
      array.Append(std::move(item));
    }
  } else {
    object.Set("error", Json::String(listed.status().message()));
  }
  object.Set("devices", std::move(array));
  return NewJvmString(env, object.Serialize());
}

JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeBeginPairing(
    JNIEnv* env, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("{}");
  Result<PairingInfo> info = session->controller->BeginPairing();
  Json object = Json::Object();
  if (!info.ok()) {
    object.Set("error", Json::String(info.status().message()));
    return NewJvmString(env, object.Serialize());
  }
  object.Set("public_key", Json::String(info.value().public_key_base64url));
  object.Set("key_fingerprint", Json::String(info.value().key_fingerprint));
  object.Set("server_url", Json::String(info.value().server_url));
  object.Set("device_name", Json::String(info.value().device_name));
  return NewJvmString(env, object.Serialize());
}

JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeCompletePairing(
    JNIEnv* env, jobject, jlong handle, jstring identity_id, jstring device_id) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("no session");
  Status status = session->controller->CompletePairing(ToStdString(env, identity_id),
                                                       ToStdString(env, device_id));
  return status.ok() ? env->NewStringUTF("") : NewJvmString(env, status.message());
}

/// Blocking: several HTTP requests. Kotlin calls it off the UI thread.
JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeCompletePairingWithCode(
    JNIEnv* env, jobject, jlong handle, jstring code) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("{\"error\":\"no session\"}");
  Result<std::string> paired =
      session->controller->CompletePairingWithCode(ToStdString(env, code));
  Json object = Json::Object();
  if (paired.ok()) {
    object.Set("server_url", Json::String(paired.value()));
  } else {
    object.Set("error", Json::String(paired.status().message()));
  }
  return NewJvmString(env, object.Serialize());
}

JNIEXPORT jboolean JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeHasCredentials(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  return session != nullptr && session->controller->HasCredentials() ? JNI_TRUE : JNI_FALSE;
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeForgetCredentials(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->controller->ForgetCredentials();
}

// --------------------------------------------------------------------- the view

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeZoomBy(
    JNIEnv*, jobject, jlong handle, jfloat factor, jfloat focus_x, jfloat focus_y) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->render_thread->ZoomBy(factor, focus_x, focus_y);
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativePanBy(
    JNIEnv*, jobject, jlong handle, jfloat dx, jfloat dy) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->render_thread->PanBy(dx, dy);
}

/// Returns to the newest output and keeps up with it: both the scrollback position and
/// the view onto the grid, which are the two independent ways of being somewhere else
/// (spec §5.2, §10.4).
///
/// Both halves are commanded from the UI thread into one FIFO, so the last intent
/// always wins on both. The scrollback half is queued, so it lands a frame or two after
/// the view half; nothing reports success until a snapshot says the session really is
/// at the bottom. Returns false when the queue refused the request, which spec §6.2
/// requires be reported rather than dropped — the view is then left alone rather than
/// claiming to follow output it never asked for.
/// Turns following on or off (spec §5.2).
///
/// Turning it *on* is `nativeFollowLatest`: it has to move the scrollback as well as the
/// view. Turning it off is only ever a change of intent, so it cannot fail.
JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeStopFollowing(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return;
  session->render_thread->SetFollowOutput(false);
}

JNIEXPORT jboolean JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeFollowLatest(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return JNI_FALSE;
  if (!session->controller->ScrollToBottom()) return JNI_FALSE;
  session->render_thread->SetFollowOutput(true);
  return JNI_TRUE;
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeResetView(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr) session->render_thread->ResetView();
}

/// Terminal pixels per surface pixel. The scrollback drag needs it: at twice the zoom
/// a finger travels half as many rows.
JNIEXPORT jfloat JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeViewScale(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return 1.0f;
  return session->render_thread->view().scale;
}

/// The cell under a point on the surface, packed as (row << 32) | column, or -1 when
/// the point is outside the terminal.
///
/// Done natively because the mapping needs the zoom, the pan and the cell metrics,
/// and splitting it across the JNI boundary is how selection ends up one cell off.
JNIEXPORT jlong JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeCellAt(
    JNIEnv*, jobject, jlong handle, jfloat x, jfloat y) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return -1;
  float terminal_x = 0.0f;
  float terminal_y = 0.0f;
  if (!session->render_thread->SurfaceToTerminal(x, y, &terminal_x, &terminal_y)) {
    return -1;
  }
  const tmirror::render::CellMetrics metrics = session->render_thread->metrics();
  if (metrics.cell_width <= 0.0f || metrics.cell_height <= 0.0f) return -1;
  const jlong column = static_cast<jlong>(terminal_x / metrics.cell_width);
  const jlong row = static_cast<jlong>(terminal_y / metrics.cell_height);
  return (row << 32) | (column & 0xFFFFFFFFLL);
}

// ------------------------------------------------------------------- the tunnel

JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeTunnelStatus(
    JNIEnv* env, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  tmirror::net::TailscaleStatus status;
  if (session != nullptr && session->tunnel) {
    status = session->tunnel->GetStatus();
  } else if (session != nullptr) {
    // Configured off: the node is not merely stopped, it is not part of this session.
    status.backend_state = "Disabled";
  }
  return NewJvmString(env, TunnelStatusToJson(status));
}

/// Starts the node. `auth_key` may be empty, in which case the status carries a login
/// URL once the coordination server issues one.
JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeTunnelStart(
    JNIEnv* env, jobject, jlong handle, jstring auth_key) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("no session");
  if (!session->tunnel) return env->NewStringUTF("the tunnel is switched off");
  Status status = session->tunnel->Start(ToStdString(env, auth_key));
  return status.ok() ? env->NewStringUTF("") : NewJvmString(env, status.message());
}

JNIEXPORT void JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeTunnelStop(
    JNIEnv*, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session != nullptr && session->tunnel) session->tunnel->Stop();
}

JNIEXPORT jstring JNICALL Java_com_hypedriven_hypeterm_NativeBridge_nativeTunnelLogout(
    JNIEnv* env, jobject, jlong handle) {
  NativeSession* session = FromHandle(handle);
  if (session == nullptr) return env->NewStringUTF("no session");
  if (!session->tunnel) return env->NewStringUTF("");
  Status status = session->tunnel->Logout();
  return status.ok() ? env->NewStringUTF("") : NewJvmString(env, status.message());
}

}  // extern "C"
