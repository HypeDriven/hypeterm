#pragma once

#include <functional>
#include <string>
#include <vector>

namespace tmtest {

/// A deliberately small test framework.
///
/// The project has no package manager and the toolchain that builds it on a developer
/// machine is plain CMake plus a compiler, so pulling in a test library would be the
/// heaviest dependency in the tree. Everything here is one registry, a few macros and
/// a main().
struct TestCase {
  std::string suite;
  std::string name;
  std::function<void()> body;
};

class Registry {
 public:
  static Registry& Instance();
  void Add(TestCase test);
  const std::vector<TestCase>& tests() const { return tests_; }

 private:
  std::vector<TestCase> tests_;
};

struct Registrar {
  Registrar(const char* suite, const char* name, std::function<void()> body);
};

/// Records a failure for the running test and keeps going, so one test reports every
/// problem it finds rather than only the first.
void ReportFailure(const char* file, int line, const std::string& message);
/// Aborts the current test immediately.
void ReportFatal(const char* file, int line, const std::string& message);

int RunAll(int argc, char** argv);

std::string Describe(bool value);
std::string Describe(int value);
std::string Describe(long value);
std::string Describe(long long value);
std::string Describe(unsigned value);
std::string Describe(unsigned long value);
std::string Describe(unsigned long long value);
std::string Describe(double value);
std::string Describe(char value);
std::string Describe(const char* value);
std::string Describe(const std::string& value);
std::string Describe(const std::u32string& value);

template <typename T>
std::string Describe(const T& value) {
  (void)value;
  return "<value>";
}

}  // namespace tmtest

#define TM_TEST(suite, name)                                                          \
  static void suite##_##name##_body();                                                \
  static ::tmtest::Registrar suite##_##name##_registrar(#suite, #name,                \
                                                        suite##_##name##_body);       \
  static void suite##_##name##_body()

#define TM_CHECK(condition)                                                           \
  do {                                                                                \
    if (!(condition)) {                                                               \
      ::tmtest::ReportFailure(__FILE__, __LINE__, "expected: " #condition);           \
    }                                                                                 \
  } while (false)

#define TM_CHECK_MSG(condition, message)                                              \
  do {                                                                                \
    if (!(condition)) {                                                               \
      ::tmtest::ReportFailure(__FILE__, __LINE__,                                     \
                              std::string("expected: " #condition " — ") + (message)); \
    }                                                                                 \
  } while (false)

#define TM_CHECK_EQ(actual, expected)                                                 \
  do {                                                                                \
    auto&& tm_actual = (actual);                                                      \
    auto&& tm_expected = (expected);                                                  \
    if (!(tm_actual == tm_expected)) {                                                \
      ::tmtest::ReportFailure(__FILE__, __LINE__,                                     \
                              std::string(#actual " == " #expected "\n    actual:   ") + \
                                  ::tmtest::Describe(tm_actual) +                     \
                                  "\n    expected: " + ::tmtest::Describe(tm_expected)); \
    }                                                                                 \
  } while (false)

#define TM_CHECK_NE(actual, expected)                                                 \
  do {                                                                                \
    auto&& tm_actual = (actual);                                                      \
    auto&& tm_expected = (expected);                                                  \
    if (tm_actual == tm_expected) {                                                   \
      ::tmtest::ReportFailure(__FILE__, __LINE__, #actual " != " #expected);          \
    }                                                                                 \
  } while (false)

/// Floating-point comparison with an explicit tolerance. There is no sensible default
/// here — a pixel offset and a scale factor need very different ones — so the caller
/// always states it.
#define TM_CHECK_NEAR(actual, expected, tolerance)                                    \
  do {                                                                                \
    const double tm_actual = static_cast<double>(actual);                             \
    const double tm_expected = static_cast<double>(expected);                         \
    const double tm_tolerance = static_cast<double>(tolerance);                       \
    const double tm_difference =                                                      \
        tm_actual > tm_expected ? tm_actual - tm_expected : tm_expected - tm_actual;  \
    if (!(tm_difference <= tm_tolerance)) {                                           \
      ::tmtest::ReportFailure(__FILE__, __LINE__,                                     \
                              std::string(#actual " ~= " #expected "\n    actual:   ") + \
                                  ::tmtest::Describe(tm_actual) +                     \
                                  "\n    expected: " + ::tmtest::Describe(tm_expected) + \
                                  " ± " + ::tmtest::Describe(tm_tolerance));          \
    }                                                                                 \
  } while (false)

#define TM_REQUIRE(condition)                                                         \
  do {                                                                                \
    if (!(condition)) {                                                               \
      ::tmtest::ReportFatal(__FILE__, __LINE__, "required: " #condition);             \
      return;                                                                         \
    }                                                                                 \
  } while (false)
