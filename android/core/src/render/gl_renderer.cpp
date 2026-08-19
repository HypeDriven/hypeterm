#include "tm/render/gl_renderer.h"

#if defined(TM_ENABLE_GLES)

#include <EGL/egl.h>
#include <GLES3/gl3.h>
#include <android/native_window.h>

#include <cstring>

#include "tm/util/log.h"

namespace tmirror {
namespace render {
namespace {

constexpr const char kTag[] = "tm.gl";

constexpr const char kSolidVertexShader[] = R"(#version 300 es
precision highp float;
layout(location = 0) in vec2 a_position;
layout(location = 1) in vec4 a_color;
uniform vec2 u_viewport;
out vec4 v_color;
void main() {
  vec2 ndc = vec2(a_position.x / u_viewport.x * 2.0 - 1.0,
                  1.0 - a_position.y / u_viewport.y * 2.0);
  gl_Position = vec4(ndc, 0.0, 1.0);
  v_color = a_color;
}
)";

constexpr const char kBlitVertexShader[] = R"(#version 300 es
precision highp float;
layout(location = 0) in vec2 a_position;   // already in normalised device coordinates
layout(location = 1) in vec2 a_texcoord;
out vec2 v_texcoord;
void main() {
  gl_Position = vec4(a_position, 0.0, 1.0);
  v_texcoord = a_texcoord;
}
)";

constexpr const char kBlitFragmentShader[] = R"(#version 300 es
precision mediump float;
in vec2 v_texcoord;
uniform sampler2D u_source;
out vec4 fragColor;
void main() {
  fragColor = texture(u_source, v_texcoord);
}
)";

constexpr const char kSolidFragmentShader[] = R"(#version 300 es
precision mediump float;
in vec4 v_color;
out vec4 fragColor;
void main() {
  // Premultiplied output, matching the blend function set in Draw (spec §10.1).
  fragColor = vec4(v_color.rgb * v_color.a, v_color.a);
}
)";

constexpr const char kGlyphVertexShader[] = R"(#version 300 es
precision highp float;
layout(location = 0) in vec2 a_position;
layout(location = 1) in vec2 a_texcoord;
layout(location = 2) in vec4 a_color;
uniform vec2 u_viewport;
out vec2 v_texcoord;
out vec4 v_color;
void main() {
  vec2 ndc = vec2(a_position.x / u_viewport.x * 2.0 - 1.0,
                  1.0 - a_position.y / u_viewport.y * 2.0);
  gl_Position = vec4(ndc, 0.0, 1.0);
  v_texcoord = a_texcoord;
  v_color = a_color;
}
)";

constexpr const char kGlyphFragmentShader[] = R"(#version 300 es
precision mediump float;
in vec2 v_texcoord;
in vec4 v_color;
uniform sampler2D u_atlas;
out vec4 fragColor;
void main() {
  float coverage = texture(u_atlas, v_texcoord).r;
  float alpha = coverage * v_color.a;
  fragColor = vec4(v_color.rgb * alpha, alpha);
}
)";

GLuint CompileShader(GLenum type, const char* source) {
  GLuint shader = glCreateShader(type);
  if (shader == 0) return 0;
  glShaderSource(shader, 1, &source, nullptr);
  glCompileShader(shader);
  GLint compiled = 0;
  glGetShaderiv(shader, GL_COMPILE_STATUS, &compiled);
  if (compiled == GL_FALSE) {
    char log[512];
    GLsizei length = 0;
    glGetShaderInfoLog(shader, sizeof(log), &length, log);
    TM_LOG_ERROR(kTag, "shader compilation failed: %.*s", static_cast<int>(length), log);
    glDeleteShader(shader);
    return 0;
  }
  return shader;
}

GLuint LinkProgram(const char* vertex_source, const char* fragment_source) {
  GLuint vertex = CompileShader(GL_VERTEX_SHADER, vertex_source);
  if (vertex == 0) return 0;
  GLuint fragment = CompileShader(GL_FRAGMENT_SHADER, fragment_source);
  if (fragment == 0) {
    glDeleteShader(vertex);
    return 0;
  }
  GLuint program = glCreateProgram();
  glAttachShader(program, vertex);
  glAttachShader(program, fragment);
  glLinkProgram(program);
  glDeleteShader(vertex);
  glDeleteShader(fragment);

  GLint linked = 0;
  glGetProgramiv(program, GL_LINK_STATUS, &linked);
  if (linked == GL_FALSE) {
    char log[512];
    GLsizei length = 0;
    glGetProgramInfoLog(program, sizeof(log), &length, log);
    TM_LOG_ERROR(kTag, "program link failed: %.*s", static_cast<int>(length), log);
    glDeleteProgram(program);
    return 0;
  }
  return program;
}

}  // namespace

// --------------------------------------------------------------------- EglSurface

EglSurface::EglSurface() = default;

EglSurface::~EglSurface() { Shutdown(); }

Status EglSurface::Initialize() {
  if (display_ != nullptr && context_ != nullptr) return Status::Ok();
  context_lost_ = false;

  EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
  if (display == EGL_NO_DISPLAY) {
    return Status::Error(ErrorKind::kInternal, "egl: no default display");
  }
  if (eglInitialize(display, nullptr, nullptr) != EGL_TRUE) {
    return Status::Error(ErrorKind::kInternal, "egl: initialisation failed");
  }

  const EGLint config_attributes[] = {EGL_RENDERABLE_TYPE,
                                      EGL_OPENGL_ES3_BIT,
                                      EGL_SURFACE_TYPE,
                                      EGL_WINDOW_BIT,
                                      EGL_RED_SIZE,
                                      8,
                                      EGL_GREEN_SIZE,
                                      8,
                                      EGL_BLUE_SIZE,
                                      8,
                                      EGL_ALPHA_SIZE,
                                      8,
                                      EGL_DEPTH_SIZE,
                                      0,
                                      EGL_STENCIL_SIZE,
                                      0,
                                      EGL_NONE};
  EGLConfig config = nullptr;
  EGLint config_count = 0;
  if (eglChooseConfig(display, config_attributes, &config, 1, &config_count) != EGL_TRUE ||
      config_count < 1) {
    return Status::Error(ErrorKind::kInternal, "egl: no suitable configuration");
  }

  const EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
  EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
  if (context == EGL_NO_CONTEXT) {
    return Status::Error(ErrorKind::kInternal, "egl: cannot create an ES 3.0 context");
  }

  display_ = display;
  config_ = config;
  context_ = context;
  return Status::Ok();
}

Status EglSurface::SetWindow(ANativeWindow* window) {
  if (display_ == nullptr) {
    Status status = Initialize();
    if (!status.ok()) return status;
  }
  EGLDisplay display = static_cast<EGLDisplay>(display_);

  if (surface_ != nullptr) {
    eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroySurface(display, static_cast<EGLSurface>(surface_));
    surface_ = nullptr;
  }
  if (window_ != nullptr) {
    ANativeWindow_release(window_);
    window_ = nullptr;
  }
  width_ = 0;
  height_ = 0;
  if (window == nullptr) return Status::Ok();  // surface loss: keep the context

  ANativeWindow_acquire(window);
  window_ = window;
  EGLSurface surface = eglCreateWindowSurface(display, static_cast<EGLConfig>(config_),
                                              window, nullptr);
  if (surface == EGL_NO_SURFACE) {
    ANativeWindow_release(window_);
    window_ = nullptr;
    return Status::Error(ErrorKind::kInternal, "egl: cannot create a window surface");
  }
  surface_ = surface;

  Status current = MakeCurrent();
  if (!current.ok()) return current;

  EGLint width = 0;
  EGLint height = 0;
  eglQuerySurface(display, surface, EGL_WIDTH, &width);
  eglQuerySurface(display, surface, EGL_HEIGHT, &height);
  width_ = width;
  height_ = height;
  return Status::Ok();
}

Status EglSurface::MakeCurrent() {
  if (display_ == nullptr || context_ == nullptr || surface_ == nullptr) {
    return Status::Error(ErrorKind::kInternal, "egl: no surface to make current");
  }
  if (eglMakeCurrent(static_cast<EGLDisplay>(display_), static_cast<EGLSurface>(surface_),
                     static_cast<EGLSurface>(surface_),
                     static_cast<EGLContext>(context_)) != EGL_TRUE) {
    EGLint error = eglGetError();
    if (error == EGL_CONTEXT_LOST) {
      context_lost_ = true;
      return Status::Error(ErrorKind::kInternal, "egl: context lost");
    }
    return Status::Error(ErrorKind::kInternal, "egl: cannot make the context current");
  }
  return Status::Ok();
}

Status EglSurface::SwapBuffers() {
  if (display_ == nullptr || surface_ == nullptr) {
    return Status::Error(ErrorKind::kInternal, "egl: nothing to present");
  }
  if (eglSwapBuffers(static_cast<EGLDisplay>(display_), static_cast<EGLSurface>(surface_)) ==
      EGL_TRUE) {
    return Status::Ok();
  }
  EGLint error = eglGetError();
  if (error == EGL_CONTEXT_LOST) {
    // The terminal model is untouched; only GPU objects are gone (spec §10.1).
    context_lost_ = true;
    eglDestroyContext(static_cast<EGLDisplay>(display_), static_cast<EGLContext>(context_));
    context_ = nullptr;
    return Status::Error(ErrorKind::kInternal, "egl: context lost");
  }
  if (error == EGL_BAD_SURFACE || error == EGL_BAD_NATIVE_WINDOW) {
    eglDestroySurface(static_cast<EGLDisplay>(display_), static_cast<EGLSurface>(surface_));
    surface_ = nullptr;
    return Status::Error(ErrorKind::kInternal, "egl: surface is no longer valid");
  }
  return Status::Error(ErrorKind::kInternal, "egl: swap failed");
}

void EglSurface::Shutdown() {
  if (display_ != nullptr) {
    EGLDisplay display = static_cast<EGLDisplay>(display_);
    eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    if (surface_ != nullptr) eglDestroySurface(display, static_cast<EGLSurface>(surface_));
    if (context_ != nullptr) eglDestroyContext(display, static_cast<EGLContext>(context_));
    eglTerminate(display);
  }
  if (window_ != nullptr) ANativeWindow_release(window_);
  display_ = nullptr;
  context_ = nullptr;
  surface_ = nullptr;
  config_ = nullptr;
  window_ = nullptr;
  width_ = 0;
  height_ = 0;
}

// --------------------------------------------------------------------- GlRenderer

GlRenderer::GlRenderer() = default;

GlRenderer::~GlRenderer() = default;

Status GlRenderer::Initialize() {
  if (initialized_) return Status::Ok();

  solid_program_ = LinkProgram(kSolidVertexShader, kSolidFragmentShader);
  glyph_program_ = LinkProgram(kGlyphVertexShader, kGlyphFragmentShader);
  blit_program_ = LinkProgram(kBlitVertexShader, kBlitFragmentShader);
  if (solid_program_ == 0 || glyph_program_ == 0 || blit_program_ == 0) {
    return Status::Error(ErrorKind::kInternal, "gl: shader programs failed to build");
  }
  blit_sampler_ = static_cast<std::uint32_t>(glGetUniformLocation(blit_program_, "u_source"));
  solid_projection_ =
      static_cast<std::uint32_t>(glGetUniformLocation(solid_program_, "u_viewport"));
  glyph_projection_ =
      static_cast<std::uint32_t>(glGetUniformLocation(glyph_program_, "u_viewport"));
  glyph_sampler_ = static_cast<std::uint32_t>(glGetUniformLocation(glyph_program_, "u_atlas"));

  GLuint buffer = 0;
  glGenBuffers(1, &buffer);
  vertex_buffer_ = buffer;

  GLuint vaos[3] = {0, 0, 0};
  glGenVertexArrays(3, vaos);
  solid_vao_ = vaos[0];
  glyph_vao_ = vaos[1];
  blit_vao_ = vaos[2];

  glBindVertexArray(solid_vao_);
  glBindBuffer(GL_ARRAY_BUFFER, vertex_buffer_);
  glEnableVertexAttribArray(0);
  glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 6 * sizeof(float), nullptr);
  glEnableVertexAttribArray(1);
  glVertexAttribPointer(1, 4, GL_FLOAT, GL_FALSE, 6 * sizeof(float),
                        reinterpret_cast<const void*>(2 * sizeof(float)));

  glBindVertexArray(glyph_vao_);
  glBindBuffer(GL_ARRAY_BUFFER, vertex_buffer_);
  glEnableVertexAttribArray(0);
  glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 8 * sizeof(float), nullptr);
  glEnableVertexAttribArray(1);
  glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 8 * sizeof(float),
                        reinterpret_cast<const void*>(2 * sizeof(float)));
  glEnableVertexAttribArray(2);
  glVertexAttribPointer(2, 4, GL_FLOAT, GL_FALSE, 8 * sizeof(float),
                        reinterpret_cast<const void*>(4 * sizeof(float)));
  glBindVertexArray(0);
  // The blit takes normalised device coordinates and a texture coordinate: four
  // floats per vertex, and no colour.
  glBindVertexArray(blit_vao_);
  glBindBuffer(GL_ARRAY_BUFFER, vertex_buffer_);
  glEnableVertexAttribArray(0);
  glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 4 * sizeof(float), nullptr);
  glEnableVertexAttribArray(1);
  glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 4 * sizeof(float),
                        reinterpret_cast<const void*>(2 * sizeof(float)));


  atlas_textures_.clear();
  atlas_generation_ = 0;
  initialized_ = true;
  return Status::Ok();
}

void GlRenderer::OnContextLost() {
  // The names are already invalid; deleting them would be undefined.
  solid_program_ = 0;
  glyph_program_ = 0;
  vertex_buffer_ = 0;
  solid_vao_ = 0;
  glyph_vao_ = 0;
  atlas_textures_.clear();
  atlas_generation_ = 0;
  initialized_ = false;
  target_framebuffer_ = 0;
  target_texture_ = 0;
  target_width_ = 0;
  target_height_ = 0;
  blit_program_ = 0;
  blit_vao_ = 0;
}

void GlRenderer::SetViewport(int width, int height) {
  viewport_width_ = width;
  viewport_height_ = height;
  glViewport(0, 0, width, height);
}

bool GlRenderer::EnsureAtlasTextures(const GlyphAtlas& atlas) {
  const std::size_t page_count = atlas.pages().size();
  bool regenerate = atlas.generation() != atlas_generation_ ||
                    atlas_textures_.size() != page_count;
  if (regenerate) {
    if (!atlas_textures_.empty()) {
      glDeleteTextures(static_cast<GLsizei>(atlas_textures_.size()), atlas_textures_.data());
    }
    atlas_textures_.assign(page_count, 0);
    glGenTextures(static_cast<GLsizei>(page_count), atlas_textures_.data());
    glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
    for (std::size_t i = 0; i < page_count; ++i) {
      glBindTexture(GL_TEXTURE_2D, atlas_textures_[i]);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
      glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
      glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, atlas.page_size(), atlas.page_size(), 0, GL_RED,
                   GL_UNSIGNED_BYTE, atlas.pages()[i].pixels.data());
    }
    atlas_generation_ = atlas.generation();
    return true;
  }

  // Incremental upload of the dirty rectangle only.
  glPixelStorei(GL_UNPACK_ALIGNMENT, 1);
  for (std::size_t i = 0; i < page_count; ++i) {
    const AtlasPage& page = atlas.pages()[i];
    if (!page.dirty) continue;
    glBindTexture(GL_TEXTURE_2D, atlas_textures_[i]);
    // A sub-image upload needs contiguous rows, so the full-width band that covers
    // the dirty rectangle is uploaded rather than an arbitrary rectangle.
    const int y0 = page.dirty_y0;
    const int rows = page.dirty_y1 - page.dirty_y0;
    if (rows > 0) {
      glTexSubImage2D(GL_TEXTURE_2D, 0, 0, y0, atlas.page_size(), rows, GL_RED,
                      GL_UNSIGNED_BYTE,
                      page.pixels.data() +
                          static_cast<std::size_t>(y0) *
                              static_cast<std::size_t>(atlas.page_size()));
    }
  }
  return true;
}

void GlRenderer::DrawSolidQuads(const std::vector<Quad>& quads) {
  if (quads.empty()) return;
  scratch_.clear();
  scratch_.reserve(quads.size() * 6 * 6);
  for (const Quad& quad : quads) {
    const float r = static_cast<float>(quad.color.r) / 255.0f;
    const float g = static_cast<float>(quad.color.g) / 255.0f;
    const float b = static_cast<float>(quad.color.b) / 255.0f;
    const float a = static_cast<float>(quad.color.a) / 255.0f;
    const float x0 = quad.x;
    const float y0 = quad.y;
    const float x1 = quad.x + quad.width;
    const float y1 = quad.y + quad.height;
    const float vertices[6][6] = {
        {x0, y0, r, g, b, a}, {x1, y0, r, g, b, a}, {x1, y1, r, g, b, a},
        {x0, y0, r, g, b, a}, {x1, y1, r, g, b, a}, {x0, y1, r, g, b, a},
    };
    for (int i = 0; i < 6; ++i) {
      for (int j = 0; j < 6; ++j) scratch_.push_back(vertices[i][j]);
    }
  }
  glUseProgram(solid_program_);
  glUniform2f(static_cast<GLint>(solid_projection_), static_cast<float>(viewport_width_),
              static_cast<float>(viewport_height_));
  glBindVertexArray(solid_vao_);
  glBindBuffer(GL_ARRAY_BUFFER, vertex_buffer_);
  glBufferData(GL_ARRAY_BUFFER,
               static_cast<GLsizeiptr>(scratch_.size() * sizeof(float)), scratch_.data(),
               GL_STREAM_DRAW);
  glDrawArrays(GL_TRIANGLES, 0, static_cast<GLsizei>(scratch_.size() / 6));
}

void GlRenderer::DrawGlyphQuads(const std::vector<GlyphQuad>& quads, const GlyphAtlas& atlas) {
  if (quads.empty()) return;
  glUseProgram(glyph_program_);
  glUniform2f(static_cast<GLint>(glyph_projection_), static_cast<float>(viewport_width_),
              static_cast<float>(viewport_height_));
  glUniform1i(static_cast<GLint>(glyph_sampler_), 0);
  glActiveTexture(GL_TEXTURE0);
  glBindVertexArray(glyph_vao_);

  // Quads are grouped by atlas page so each page is one draw call.
  for (std::size_t page = 0; page < atlas_textures_.size(); ++page) {
    scratch_.clear();
    for (const GlyphQuad& quad : quads) {
      if (static_cast<std::size_t>(quad.page) != page) continue;
      const float r = static_cast<float>(quad.color.r) / 255.0f;
      const float g = static_cast<float>(quad.color.g) / 255.0f;
      const float b = static_cast<float>(quad.color.b) / 255.0f;
      const float a = static_cast<float>(quad.color.a) / 255.0f;
      const float x0 = quad.x;
      const float y0 = quad.y;
      const float x1 = quad.x + quad.width;
      const float y1 = quad.y + quad.height;
      const float vertices[6][8] = {
          {x0, y0, quad.u0, quad.v0, r, g, b, a}, {x1, y0, quad.u1, quad.v0, r, g, b, a},
          {x1, y1, quad.u1, quad.v1, r, g, b, a}, {x0, y0, quad.u0, quad.v0, r, g, b, a},
          {x1, y1, quad.u1, quad.v1, r, g, b, a}, {x0, y1, quad.u0, quad.v1, r, g, b, a},
      };
      for (int i = 0; i < 6; ++i) {
        for (int j = 0; j < 8; ++j) scratch_.push_back(vertices[i][j]);
      }
    }
    if (scratch_.empty()) continue;
    glBindTexture(GL_TEXTURE_2D, atlas_textures_[page]);
    glBindBuffer(GL_ARRAY_BUFFER, vertex_buffer_);
    glBufferData(GL_ARRAY_BUFFER,
                 static_cast<GLsizeiptr>(scratch_.size() * sizeof(float)), scratch_.data(),
                 GL_STREAM_DRAW);
    glDrawArrays(GL_TRIANGLES, 0, static_cast<GLsizei>(scratch_.size() / 8));
  }
}

void GlRenderer::DrawFrameLayers(const RenderFrame& frame, const GlyphAtlas& atlas) {
  glDisable(GL_DEPTH_TEST);
  glDisable(GL_SCISSOR_TEST);
  glEnable(GL_BLEND);
  // Premultiplied alpha (spec §10.1).
  glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);

  glClearColor(static_cast<float>(frame.background.r) / 255.0f,
               static_cast<float>(frame.background.g) / 255.0f,
               static_cast<float>(frame.background.b) / 255.0f, 1.0f);
  glClear(GL_COLOR_BUFFER_BIT);

  EnsureAtlasTextures(atlas);

  // Order is fixed (spec §10.1) and shared with the CPU reference renderer.
  DrawSolidQuads(frame.backgrounds);
  DrawGlyphQuads(frame.glyphs, atlas);
  DrawSolidQuads(frame.decorations);
  DrawSolidQuads(frame.cursor);
  DrawGlyphQuads(frame.cursor_glyphs, atlas);

  glBindVertexArray(0);
  glUseProgram(0);
}

void GlRenderer::Draw(const RenderFrame& frame, const GlyphAtlas& atlas) {
  if (!initialized_) return;
  DrawFrameLayers(frame, atlas);
}

bool GlRenderer::EnsureTarget(int width, int height) {
  if (width <= 0 || height <= 0) return false;
  if (target_texture_ != 0 && width == target_width_ && height == target_height_) {
    return true;
  }
  if (target_texture_ != 0) {
    GLuint texture = target_texture_;
    glDeleteTextures(1, &texture);
    target_texture_ = 0;
  }
  if (target_framebuffer_ != 0) {
    GLuint framebuffer = target_framebuffer_;
    glDeleteFramebuffers(1, &framebuffer);
    target_framebuffer_ = 0;
  }

  GLuint texture = 0;
  glGenTextures(1, &texture);
  glBindTexture(GL_TEXTURE_2D, texture);
  glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, width, height, 0, GL_RGBA, GL_UNSIGNED_BYTE,
               nullptr);
  // Mipmaps are what make the zoomed-out overview readable rather than a shimmering
  // mess: at a quarter scale, point sampling drops three of every four rows of pixels
  // and text stops resolving at all.
  glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR_MIPMAP_LINEAR);
  glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
  glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
  glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

  GLuint framebuffer = 0;
  glGenFramebuffers(1, &framebuffer);
  glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
  glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
  const GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
  glBindFramebuffer(GL_FRAMEBUFFER, 0);

  if (status != GL_FRAMEBUFFER_COMPLETE) {
    glDeleteFramebuffers(1, &framebuffer);
    glDeleteTextures(1, &texture);
    return false;
  }

  target_texture_ = texture;
  target_framebuffer_ = framebuffer;
  target_width_ = width;
  target_height_ = height;
  return true;
}

Status GlRenderer::RenderToTexture(const RenderFrame& frame, const GlyphAtlas& atlas) {
  if (!initialized_) {
    return Status::Error(ErrorKind::kInternal, "gl: renderer is not initialised");
  }

  int width = static_cast<int>(frame.width_px + 0.5f);
  int height = static_cast<int>(frame.height_px + 0.5f);
  if (width <= 0 || height <= 0) {
    return Status::Error(ErrorKind::kInvalidArgument, "gl: the frame has no area");
  }

  // A large terminal can exceed what the GPU will hold — 500 columns at ten pixels is
  // already past some limits. Rendering smaller keeps it visible; refusing would not.
  GLint max_texture = 2048;
  glGetIntegerv(GL_MAX_TEXTURE_SIZE, &max_texture);
  target_scale_ = 1.0f;
  const int largest = width > height ? width : height;
  if (largest > max_texture) {
    target_scale_ = static_cast<float>(max_texture) / static_cast<float>(largest);
    width = static_cast<int>(static_cast<float>(width) * target_scale_);
    height = static_cast<int>(static_cast<float>(height) * target_scale_);
    if (width <= 0) width = 1;
    if (height <= 0) height = 1;
  }

  if (!EnsureTarget(width, height)) {
    return Status::Error(ErrorKind::kInternal, "gl: no offscreen render target");
  }

  glBindFramebuffer(GL_FRAMEBUFFER, target_framebuffer_);
  const int previous_width = viewport_width_;
  const int previous_height = viewport_height_;

  // Two separate mappings, and conflating them is the easy mistake here. The shaders
  // turn frame coordinates into normalised device coordinates using u_viewport, so
  // that must be the *frame's* own extent for the whole grid to land in view.
  // glViewport then maps those coordinates onto the texture, which is where any
  // downscaling happens — so no special case is needed for an oversized grid.
  viewport_width_ = static_cast<int>(frame.width_px + 0.5f);
  viewport_height_ = static_cast<int>(frame.height_px + 0.5f);
  glViewport(0, 0, target_width_, target_height_);

  DrawFrameLayers(frame, atlas);

  glBindTexture(GL_TEXTURE_2D, target_texture_);
  glGenerateMipmap(GL_TEXTURE_2D);

  glBindFramebuffer(GL_FRAMEBUFFER, 0);
  SetViewport(previous_width, previous_height);
  return Status::Ok();
}

void GlRenderer::Present(const ViewTransform& view, Rgba background) {
  if (!initialized_ || target_texture_ == 0) return;

  glBindFramebuffer(GL_FRAMEBUFFER, 0);
  glViewport(0, 0, viewport_width_, viewport_height_);
  glDisable(GL_DEPTH_TEST);
  glDisable(GL_SCISSOR_TEST);
  glDisable(GL_BLEND);

  // The area outside the terminal is the terminal's own background, so a grid smaller
  // than the screen looks like part of the same surface rather than a letterboxed image.
  glClearColor(static_cast<float>(background.r) / 255.0f,
               static_cast<float>(background.g) / 255.0f,
               static_cast<float>(background.b) / 255.0f, 1.0f);
  glClear(GL_COLOR_BUFFER_BIT);

  const float width = static_cast<float>(target_width_) * view.scale;
  const float height = static_cast<float>(target_height_) * view.scale;
  const float left = view.offset_x;
  const float top = view.offset_y;
  const float right = left + width;
  const float bottom = top + height;

  const float viewport_w = static_cast<float>(viewport_width_);
  const float viewport_h = static_cast<float>(viewport_height_);
  if (viewport_w <= 0.0f || viewport_h <= 0.0f) return;

  auto to_ndc_x = [&](float x) { return x / viewport_w * 2.0f - 1.0f; };
  auto to_ndc_y = [&](float y) { return 1.0f - y / viewport_h * 2.0f; };

  const float x0 = to_ndc_x(left);
  const float x1 = to_ndc_x(right);
  const float y0 = to_ndc_y(top);
  const float y1 = to_ndc_y(bottom);

  // Texture coordinates have their origin at the bottom left, so v is flipped.
  const float vertices[] = {
      x0, y0, 0.0f, 1.0f, x1, y0, 1.0f, 1.0f, x1, y1, 1.0f, 0.0f,
      x0, y0, 0.0f, 1.0f, x1, y1, 1.0f, 0.0f, x0, y1, 0.0f, 0.0f,
  };

  glUseProgram(blit_program_);
  glUniform1i(static_cast<GLint>(blit_sampler_), 0);
  glActiveTexture(GL_TEXTURE0);
  glBindTexture(GL_TEXTURE_2D, target_texture_);
  glBindVertexArray(blit_vao_);
  glBindBuffer(GL_ARRAY_BUFFER, vertex_buffer_);
  glBufferData(GL_ARRAY_BUFFER, sizeof(vertices), vertices, GL_STREAM_DRAW);
  glDrawArrays(GL_TRIANGLES, 0, 6);

  glBindVertexArray(0);
  glUseProgram(0);
}

}  // namespace render
}  // namespace tmirror

#endif  // TM_ENABLE_GLES
