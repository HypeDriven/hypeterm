// JSON, base64, URL parsing, bounded queues and reconnect backoff (spec §7.4, §12,
// §16.1).

#include <string>
#include <thread>

#include "framework.h"
#include "tm/net/url.h"
#include "tm/util/backoff.h"
#include "tm/util/base64.h"
#include "tm/util/json.h"
#include "tm/util/queue.h"
#include "tm/util/strings.h"

using tmirror::Backoff;
using tmirror::BoundedQueue;
using tmirror::Base64Decode;
using tmirror::Base64Encode;
using tmirror::Base64UrlDecode;
using tmirror::Base64UrlEncode;
using tmirror::Bytes;
using tmirror::ByteView;
using tmirror::ErrorKind;
using tmirror::Json;
using tmirror::PushResult;
using tmirror::Result;
using tmirror::net::ParseUrl;
using tmirror::net::Url;

TM_TEST(Base64, RoundTripsUrlSafeWithoutPadding) {
  Bytes data = {0x00, 0x01, 0xFE, 0xFF, 0x7F};
  std::string encoded = Base64UrlEncode(ByteView(data));
  TM_CHECK(encoded.find('=') == std::string::npos);
  TM_CHECK(encoded.find('+') == std::string::npos);
  TM_CHECK(encoded.find('/') == std::string::npos);
  Bytes decoded;
  TM_CHECK(Base64UrlDecode(encoded, &decoded));
  TM_CHECK(decoded == data);
}

TM_TEST(Base64, AcceptsPaddedInputAndRejectsJunk) {
  Bytes decoded;
  TM_CHECK(Base64UrlDecode("aGVsbG8=", &decoded));
  TM_CHECK_EQ(std::string(decoded.begin(), decoded.end()), "hello");
  TM_CHECK(!Base64UrlDecode("aGVsbG8*", &decoded));
  TM_CHECK(!Base64UrlDecode("a", &decoded));  // impossible length
}

TM_TEST(Base64, StandardAlphabetForWebSocketKeys) {
  Bytes data(16, 0xAB);
  std::string encoded = Base64Encode(ByteView(data));
  TM_CHECK_EQ(encoded.size(), static_cast<std::size_t>(24));
  Bytes decoded;
  TM_CHECK(Base64Decode(encoded, &decoded));
  TM_CHECK(decoded == data);
}

TM_TEST(Json, ParsesObjectsAndKeepsIntegersExact) {
  Result<Json> parsed = Json::Parse(R"({"a":1,"b":"two","c":[1,2,3],"d":true,"e":null,
                                       "big":18446744073709551615})");
  TM_REQUIRE(parsed.ok());
  const Json& value = parsed.value();
  TM_CHECK(value.is_object());
  std::uint64_t number = 0;
  TM_CHECK(value.GetUint64("a", &number));
  TM_CHECK_EQ(number, static_cast<std::uint64_t>(1));
  TM_CHECK_EQ(value.GetString("b"), "two");
  TM_CHECK(value.GetBool("d", false));
  TM_REQUIRE(value.Find("c") != nullptr);
  TM_CHECK_EQ(value.Find("c")->items().size(), static_cast<std::size_t>(3));
  // A 64-bit offset must survive without passing through a double.
  TM_CHECK(value.GetUint64("big", &number));
  TM_CHECK_EQ(number, static_cast<std::uint64_t>(18446744073709551615ULL));
}

TM_TEST(Json, RejectsMalformedDocuments) {
  TM_CHECK(!Json::Parse("{").ok());
  TM_CHECK(!Json::Parse("{\"a\":}").ok());
  TM_CHECK(!Json::Parse("{'a':1}").ok());
  TM_CHECK(!Json::Parse("[1,2,]").ok());
  TM_CHECK(!Json::Parse("1 2").ok());
  TM_CHECK(!Json::Parse("\"unterminated").ok());
  TM_CHECK(!Json::Parse("{\"a\":01}").ok());
}

TM_TEST(Json, BoundsDepthSizeAndElementCount) {
  Json::Limits limits;
  limits.max_depth = 4;
  std::string deep;
  for (int i = 0; i < 50; ++i) deep += "[";
  for (int i = 0; i < 50; ++i) deep += "]";
  TM_CHECK(!Json::Parse(deep, limits).ok());

  limits = Json::Limits();
  limits.max_bytes = 16;
  TM_CHECK(!Json::Parse(std::string("{\"key\":\"") + std::string(100, 'x') + "\"}", limits).ok());

  limits = Json::Limits();
  limits.max_elements = 8;
  TM_CHECK(!Json::Parse("[1,2,3,4,5,6,7,8,9,10]", limits).ok());
}

TM_TEST(Json, HandlesEscapesAndSurrogatePairs) {
  Result<Json> parsed = Json::Parse(R"({"s":"a\nbA😀é"})");
  TM_REQUIRE(parsed.ok());
  TM_CHECK_EQ(parsed.value().GetString("s"), "a\nbA\xF0\x9F\x98\x80\xC3\xA9");

  // A lone high surrogate becomes the replacement character rather than failing:
  // this is a terminal label, not a security decision.
  Result<Json> lone = Json::Parse(R"({"s":"\ud83d"})");
  TM_REQUIRE(lone.ok());
  TM_CHECK_EQ(lone.value().GetString("s"), "\xEF\xBF\xBD");
}

TM_TEST(Json, SerializesRoundTrip) {
  Json object = Json::Object();
  object.Set("type", Json::String("subscribe"));
  object.Set("from_offset", Json::Uint(18446744073709551615ULL));
  object.Set("flag", Json::Bool(true));
  std::string text = object.Serialize();
  Result<Json> parsed = Json::Parse(text);
  TM_REQUIRE(parsed.ok());
  std::uint64_t offset = 0;
  TM_CHECK(parsed.value().GetUint64("from_offset", &offset));
  TM_CHECK_EQ(offset, static_cast<std::uint64_t>(18446744073709551615ULL));
  TM_CHECK(text.find("1.8446744073709552e+19") == std::string::npos);
}

TM_TEST(Json, UnknownMembersAreIgnored) {
  Result<Json> parsed = Json::Parse(R"({"type":"durable","durable_offset":5,"future":"x"})");
  TM_REQUIRE(parsed.ok());
  TM_CHECK_EQ(parsed.value().GetString("type"), "durable");
}

TM_TEST(Url, ParsesSchemeHostPortAndPath) {
  Result<Url> parsed = ParseUrl("https://relay.example:8443/v1/terminals?state=open");
  TM_REQUIRE(parsed.ok());
  TM_CHECK_EQ(parsed.value().scheme, "https");
  TM_CHECK_EQ(parsed.value().host, "relay.example");
  TM_CHECK_EQ(static_cast<int>(parsed.value().port), 8443);
  TM_CHECK_EQ(parsed.value().path, "/v1/terminals");
  TM_CHECK_EQ(parsed.value().query, "state=open");
  TM_CHECK_EQ(parsed.value().origin(), "https://relay.example:8443");
}

TM_TEST(Url, AppliesDefaultPortsAndLowercasesTheHost) {
  Result<Url> https = ParseUrl("https://Relay.Example");
  TM_REQUIRE(https.ok());
  TM_CHECK_EQ(static_cast<int>(https.value().port), 443);
  TM_CHECK_EQ(https.value().host, "relay.example");
  TM_CHECK_EQ(https.value().origin(), "https://relay.example");

  Result<Url> http = ParseUrl("http://localhost");
  TM_REQUIRE(http.ok());
  TM_CHECK_EQ(static_cast<int>(http.value().port), 80);
}

TM_TEST(Url, RejectsUnsupportedAndDangerousForms) {
  TM_CHECK(!ParseUrl("relay.example").ok());
  TM_CHECK(!ParseUrl("ftp://relay.example").ok());
  TM_CHECK(!ParseUrl("https://user:pass@relay.example").ok());
  TM_CHECK(!ParseUrl("https://relay.example:0").ok());
  TM_CHECK(!ParseUrl("https://relay.example:99999").ok());
  TM_CHECK(!ParseUrl("https://").ok());
}

TM_TEST(Url, HandlesIpv6Literals) {
  Result<Url> parsed = ParseUrl("wss://[::1]:9443/v1");
  TM_REQUIRE(parsed.ok());
  TM_CHECK_EQ(parsed.value().host, "[::1]");
  TM_CHECK_EQ(static_cast<int>(parsed.value().port), 9443);
}

TM_TEST(Queue, BoundsItemsAndReportsFullness) {
  BoundedQueue<int> queue(2);
  TM_CHECK(queue.Push(1) == PushResult::kOk);
  TM_CHECK(queue.Push(2) == PushResult::kOk);
  // Full means the caller is told, never that an item disappears (spec §6.2).
  TM_CHECK(queue.Push(3) == PushResult::kFull);
  int value = 0;
  TM_CHECK(queue.TryPop(&value));
  TM_CHECK_EQ(value, 1);
  TM_CHECK(queue.Push(3) == PushResult::kOk);
}

TM_TEST(Queue, BoundsBytesAndAlwaysAcceptsOneOversizedItem) {
  BoundedQueue<std::string> queue(10, 16);
  TM_CHECK(queue.Push(std::string("a"), 10) == PushResult::kOk);
  TM_CHECK(queue.Push(std::string("b"), 10) == PushResult::kFull);
  std::string value;
  TM_CHECK(queue.TryPop(&value));
  // An item larger than the whole bound still goes through when the queue is empty,
  // otherwise a single large paste would deadlock its producer forever.
  TM_CHECK(queue.Push(std::string("huge"), 1000) == PushResult::kOk);
}

TM_TEST(Queue, DrainAllTakesEverythingForCoalescing) {
  BoundedQueue<int> queue(8);
  for (int i = 0; i < 5; ++i) queue.Push(i);
  std::vector<int> drained = queue.DrainAll();
  TM_CHECK_EQ(drained.size(), static_cast<std::size_t>(5));
  TM_CHECK_EQ(queue.size(), static_cast<std::size_t>(0));
}

TM_TEST(Queue, CloseWakesBlockedConsumers) {
  BoundedQueue<int> queue(2);
  std::thread waiter([&] {
    int value = 0;
    queue.Pop(&value);  // blocks until Close
  });
  queue.Close();
  waiter.join();
  TM_CHECK(queue.closed());
}

TM_TEST(Backoff, GrowsExponentiallyAndStaysBounded) {
  Backoff::Options options;
  options.initial_delay_ms = 100;
  options.max_delay_ms = 1000;
  options.multiplier = 2.0;
  options.jitter = 0.0;
  Backoff backoff(options);

  TM_CHECK_EQ(backoff.NextDelay(), 100);
  TM_CHECK_EQ(backoff.NextDelay(), 200);
  TM_CHECK_EQ(backoff.NextDelay(), 400);
  TM_CHECK_EQ(backoff.NextDelay(), 800);
  TM_CHECK_EQ(backoff.NextDelay(), 1000);
  TM_CHECK_EQ(backoff.NextDelay(), 1000);
}

TM_TEST(Backoff, JitterOnlyReducesTheDelay) {
  Backoff::Options options;
  options.initial_delay_ms = 1000;
  options.max_delay_ms = 1000;
  options.jitter = 0.5;
  Backoff backoff(options);
  for (int i = 0; i < 50; ++i) {
    tmirror::Millis delay = backoff.NextDelay();
    TM_CHECK(delay <= 1000);
    TM_CHECK(delay >= 500);
  }
}

TM_TEST(Backoff, ResetsOnlyAfterAStableConnection) {
  Backoff::Options options;
  options.initial_delay_ms = 100;
  options.jitter = 0.0;
  options.stability_threshold_ms = 5000;
  Backoff backoff(options);

  backoff.NextDelay();
  backoff.NextDelay();
  // A connection that dies immediately must not reset the sequence, or a server that
  // accepts and instantly drops would be hammered forever (spec §7.4).
  backoff.RecordConnected(1000);
  backoff.RecordDisconnected(1500);
  TM_CHECK_EQ(backoff.NextDelay(), 400);

  backoff.RecordConnected(2000);
  backoff.RecordDisconnected(20000);
  TM_CHECK_EQ(backoff.NextDelay(), 100);
}

TM_TEST(Strings, SanitizesMessagesForDisplayAndLogs) {
  TM_CHECK_EQ(tmirror::SanitizeForMessage("a\x1b[31mb\nc"), "a.[31mb.c");
  TM_CHECK_EQ(tmirror::SanitizeForMessage(std::string(500, 'x'), 8), "xxxxxxxx...");

  // A limit counted in bytes can land inside a multi-byte character. The half sequence
  // that would leave here goes on into a log line and across the JNI boundary, where it
  // is read as the start of a character that is not there.
  TM_CHECK_EQ(tmirror::SanitizeForMessage("aaa\xC3\xA9", 4), "aaa...");
  TM_CHECK_EQ(tmirror::SanitizeForMessage("aa\xE2\x82\xAC", 4), "aa...");
  TM_CHECK_EQ(tmirror::SanitizeForMessage("a\xF0\x9F\x98\x80", 4), "a...");
  // ...but a character the cut left intact is kept whole.
  TM_CHECK_EQ(tmirror::SanitizeForMessage("aa\xC3\xA9zz", 4), "aa\xC3\xA9...");
}

TM_TEST(Strings, ParsesBoundedIntegers) {
  std::uint64_t value = 0;
  TM_CHECK(tmirror::ParseUint64("42", 100, &value));
  TM_CHECK_EQ(value, static_cast<std::uint64_t>(42));
  TM_CHECK(!tmirror::ParseUint64("101", 100, &value));
  TM_CHECK(!tmirror::ParseUint64("", 100, &value));
  TM_CHECK(!tmirror::ParseUint64("-1", 100, &value));
  TM_CHECK(!tmirror::ParseUint64("4x", 100, &value));
  TM_CHECK(!tmirror::ParseUint64("99999999999999999999999", UINT64_MAX, &value));
}
