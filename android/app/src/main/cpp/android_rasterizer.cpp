#include "android_rasterizer.h"

#include <android/log.h>

#include <cmath>
#include <vector>

#include "tm/term/utf8.h"

namespace tmirror {
namespace android {
namespace {

constexpr const char kTag[] = "Hypeterm";

/// The Kotlin side returns one byte array: four big-endian 32-bit integers
/// (width, height, left, top) followed by the coverage bytes. One array keeps the
/// JNI surface to a single call per glyph, which matters because this runs inside a
/// frame's rasterization budget.
constexpr int kHeaderBytes = 16;

std::int32_t ReadInt32(const std::uint8_t* data) {
  return (static_cast<std::int32_t>(data[0]) << 24) |
         (static_cast<std::int32_t>(data[1]) << 16) |
         (static_cast<std::int32_t>(data[2]) << 8) | static_cast<std::int32_t>(data[3]);
}

}  // namespace

AndroidRasterizer::AndroidRasterizer(JavaVM* vm, jobject rasterizer) : vm_(vm) {
  bool attached = false;
  JNIEnv* env = AttachCurrentThread(&attached);
  if (env == nullptr) return;
  rasterizer_ = env->NewGlobalRef(rasterizer);
  jclass clazz = env->GetObjectClass(rasterizer_);
  rasterize_method_ = env->GetMethodID(clazz, "rasterize", "(Ljava/lang/String;ZZIFFFF)[B");
  measure_method_ = env->GetMethodID(clazz, "measure", "(FF)[F");
  env->DeleteLocalRef(clazz);
  if (attached) vm_->DetachCurrentThread();
}

AndroidRasterizer::~AndroidRasterizer() {
  if (rasterizer_ == nullptr) return;
  bool attached = false;
  JNIEnv* env = AttachCurrentThread(&attached);
  if (env != nullptr) env->DeleteGlobalRef(rasterizer_);
  if (attached) vm_->DetachCurrentThread();
}

JNIEnv* AndroidRasterizer::AttachCurrentThread(bool* attached) {
  *attached = false;
  JNIEnv* env = nullptr;
  jint result = vm_->GetEnv(reinterpret_cast<void**>(&env), JNI_VERSION_1_6);
  if (result == JNI_OK) return env;
  if (result != JNI_EDETACHED) return nullptr;
  // The render thread is created natively, so it starts detached.
  if (vm_->AttachCurrentThread(&env, nullptr) != JNI_OK) return nullptr;
  *attached = true;
  return env;
}

void AndroidRasterizer::Invalidate() {}

bool AndroidRasterizer::Rasterize(const render::GlyphKey& key,
                                  const render::CellMetrics& metrics,
                                  render::GlyphBitmap* out) {
  if (rasterizer_ == nullptr || rasterize_method_ == nullptr) return false;
  std::lock_guard<std::mutex> lock(mutex_);

  bool attached = false;
  JNIEnv* env = AttachCurrentThread(&attached);
  if (env == nullptr) return false;

  std::string utf8 = term::EncodeUtf8(key.cluster);
  jstring cluster = env->NewStringUTF(utf8.c_str());
  if (cluster == nullptr) {
    if (attached) vm_->DetachCurrentThread();
    return false;
  }

  jbyteArray array = static_cast<jbyteArray>(env->CallObjectMethod(
      rasterizer_, rasterize_method_, cluster, key.bold ? JNI_TRUE : JNI_FALSE,
      key.italic ? JNI_TRUE : JNI_FALSE, static_cast<jint>(key.cell_width),
      metrics.font_size_px, metrics.cell_width, metrics.cell_height, metrics.baseline));
  env->DeleteLocalRef(cluster);

  if (env->ExceptionCheck()) {
    env->ExceptionDescribe();
    env->ExceptionClear();
    if (attached) vm_->DetachCurrentThread();
    return false;
  }
  if (array == nullptr) {
    if (attached) vm_->DetachCurrentThread();
    return false;  // nothing to draw, e.g. a space
  }

  jsize length = env->GetArrayLength(array);
  bool ok = false;
  if (length > kHeaderBytes) {
    std::vector<std::uint8_t> buffer(static_cast<std::size_t>(length));
    env->GetByteArrayRegion(array, 0, length, reinterpret_cast<jbyte*>(buffer.data()));
    std::int32_t width = ReadInt32(buffer.data());
    std::int32_t height = ReadInt32(buffer.data() + 4);
    std::int32_t left = ReadInt32(buffer.data() + 8);
    std::int32_t top = ReadInt32(buffer.data() + 12);
    // Values from the JVM side are still validated: a wrong size here would read
    // past the array (spec §12 applies to every input, not only the network).
    std::size_t expected =
        static_cast<std::size_t>(kHeaderBytes) + static_cast<std::size_t>(width) *
                                                     static_cast<std::size_t>(height);
    if (width > 0 && height > 0 && width < 4096 && height < 4096 &&
        expected == static_cast<std::size_t>(length)) {
      out->width = width;
      out->height = height;
      out->left = left;
      out->top = top;
      out->alpha.assign(buffer.begin() + kHeaderBytes, buffer.end());
      ok = true;
    } else {
      __android_log_print(ANDROID_LOG_WARN, kTag, "rasterizer returned a malformed bitmap");
    }
  }
  env->DeleteLocalRef(array);
  if (attached) vm_->DetachCurrentThread();
  return ok;
}

render::CellMetrics AndroidRasterizer::MeasureCell(float font_size_px, float density) {
  render::CellMetrics metrics;
  metrics.font_size_px = font_size_px;
  metrics.density = density;
  // Sensible fallbacks if the bridge is unavailable: a wrong-looking grid beats no
  // terminal at all.
  metrics.cell_width = std::max(4.0f, std::floor(font_size_px * 0.6f));
  metrics.cell_height = std::max(6.0f, std::floor(font_size_px * 1.25f));
  metrics.baseline = std::floor(metrics.cell_height * 0.8f);
  metrics.underline_thickness = std::max(1.0f, std::floor(font_size_px / 14.0f));
  metrics.underline_position = std::max(1.0f, std::floor(metrics.cell_height * 0.12f));

  if (rasterizer_ == nullptr || measure_method_ == nullptr) return metrics;
  std::lock_guard<std::mutex> lock(mutex_);
  bool attached = false;
  JNIEnv* env = AttachCurrentThread(&attached);
  if (env == nullptr) return metrics;

  jfloatArray array = static_cast<jfloatArray>(
      env->CallObjectMethod(rasterizer_, measure_method_, font_size_px, density));
  if (env->ExceptionCheck()) {
    env->ExceptionClear();
    if (attached) vm_->DetachCurrentThread();
    return metrics;
  }
  if (array != nullptr && env->GetArrayLength(array) >= 5) {
    jfloat values[5];
    env->GetFloatArrayRegion(array, 0, 5, values);
    if (values[0] > 0.5f && values[1] > 0.5f) {
      metrics.cell_width = values[0];
      metrics.cell_height = values[1];
      metrics.baseline = values[2];
      metrics.underline_thickness = std::max(1.0f, values[3]);
      metrics.underline_position = std::max(1.0f, values[4]);
    }
  }
  if (array != nullptr) env->DeleteLocalRef(array);
  if (attached) vm_->DetachCurrentThread();
  return metrics;
}

}  // namespace android
}  // namespace tmirror
