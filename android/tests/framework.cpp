#include "framework.h"

#include "tm/util/log.h"

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

namespace tmtest {
namespace {

struct RunState {
  int failures = 0;
  bool fatal = false;
  std::string current;
};

RunState& State() {
  static RunState state;
  return state;
}

std::string Escape(const std::string& text) {
  std::string out;
  for (char c : text) {
    unsigned char u = static_cast<unsigned char>(c);
    if (u == '\x1b') {
      out += "\\e";
    } else if (u == '\n') {
      out += "\\n";
    } else if (u == '\r') {
      out += "\\r";
    } else if (u < 0x20 || u == 0x7F) {
      char buffer[8];
      std::snprintf(buffer, sizeof(buffer), "\\x%02x", u);
      out += buffer;
    } else {
      out.push_back(c);
    }
  }
  return out;
}

}  // namespace

Registry& Registry::Instance() {
  static Registry registry;
  return registry;
}

void Registry::Add(TestCase test) { tests_.push_back(std::move(test)); }

Registrar::Registrar(const char* suite, const char* name, std::function<void()> body) {
  Registry::Instance().Add(TestCase{suite, name, std::move(body)});
}

void ReportFailure(const char* file, int line, const std::string& message) {
  ++State().failures;
  std::fprintf(stderr, "  FAIL %s\n    at %s:%d\n    %s\n", State().current.c_str(), file, line,
               message.c_str());
}

void ReportFatal(const char* file, int line, const std::string& message) {
  State().fatal = true;
  ReportFailure(file, line, message);
}

std::string Describe(bool value) { return value ? "true" : "false"; }
std::string Describe(int value) { return std::to_string(value); }
std::string Describe(long value) { return std::to_string(value); }
std::string Describe(long long value) { return std::to_string(value); }
std::string Describe(unsigned value) { return std::to_string(value); }
std::string Describe(unsigned long value) { return std::to_string(value); }
std::string Describe(unsigned long long value) { return std::to_string(value); }
std::string Describe(double value) { return std::to_string(value); }
std::string Describe(char value) { return Escape(std::string(1, value)); }
std::string Describe(const char* value) {
  return value == nullptr ? "<null>" : "\"" + Escape(value) + "\"";
}
std::string Describe(const std::string& value) { return "\"" + Escape(value) + "\""; }
std::string Describe(const std::u32string& value) {
  std::string out;
  for (char32_t c : value) {
    char buffer[16];
    std::snprintf(buffer, sizeof(buffer), "U+%04X ", static_cast<unsigned>(c));
    out += buffer;
  }
  return out;
}

int RunAll(int argc, char** argv) {
  // Client logs are off unless a developer asks for them: a passing test run should
  // print its results, not a transcript.
  if (std::getenv("TM_TEST_LOG") == nullptr) {
    tmirror::Log::SetLevel(tmirror::LogLevel::kOff);
  }
  std::string filter;
  bool list_only = false;
  for (int i = 1; i < argc; ++i) {
    if (std::strcmp(argv[i], "--list") == 0) {
      list_only = true;
    } else if (std::strncmp(argv[i], "--filter=", 9) == 0) {
      filter = argv[i] + 9;
    }
  }

  const std::vector<TestCase>& tests = Registry::Instance().tests();
  int run = 0;
  int failed = 0;
  auto start = std::chrono::steady_clock::now();

  for (const TestCase& test : tests) {
    std::string full = test.suite + "." + test.name;
    if (list_only) {
      std::printf("%s\n", full.c_str());
      continue;
    }
    if (!filter.empty() && full.find(filter) == std::string::npos) continue;

    State().current = full;
    int before = State().failures;
    State().fatal = false;
    test.body();
    ++run;
    if (State().failures != before) ++failed;
  }
  if (list_only) return 0;

  auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(
                     std::chrono::steady_clock::now() - start)
                     .count();
  std::printf("\n%d test%s run, %d failed, %d assertion failure%s (%lld ms)\n", run,
              run == 1 ? "" : "s", failed, State().failures,
              State().failures == 1 ? "" : "s", static_cast<long long>(elapsed));
  return failed == 0 ? 0 : 1;
}

}  // namespace tmtest

int main(int argc, char** argv) { return tmtest::RunAll(argc, argv); }
