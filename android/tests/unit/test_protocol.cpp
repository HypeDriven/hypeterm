// Mirror protocol decoding: ordering, duplication, gaps, subscription boundaries and
// input sequencing (spec §7.3, §16.1; relay spec §6.2, §6.3).

#include <string>
#include <vector>

#include "framework.h"
#include "tm/api/mirror_session.h"
#include "tm/util/json.h"

using tmirror::ByteView;
using tmirror::Bytes;
using tmirror::ErrorKind;
using tmirror::Status;
using tmirror::api::MirrorEvent;
using tmirror::api::MirrorEventKind;
using tmirror::api::MirrorSession;
using tmirror::api::MirrorSessionConfig;

namespace {

/// Collects the normalized events a session emits.
struct Collector {
  std::vector<MirrorEvent> events;
  std::string output;

  MirrorSession::EventHandler handler() {
    return [this](const MirrorEvent& event) {
      events.push_back(event);
      if (event.kind == MirrorEventKind::kOutput) {
        output += event.payload.to_string();
        // The payload view points into the frame buffer, which does not outlive the
        // callback, so it is copied here exactly as the controller does.
        events.back().payload = ByteView();
      }
    };
  }

  const MirrorEvent* Last(MirrorEventKind kind) const {
    for (auto it = events.rbegin(); it != events.rend(); ++it) {
      if (it->kind == kind) return &*it;
    }
    return nullptr;
  }
  int Count(MirrorEventKind kind) const {
    int count = 0;
    for (const MirrorEvent& event : events) {
      if (event.kind == kind) ++count;
    }
    return count;
  }
};

Bytes OutputFrame(std::uint64_t offset, const std::string& payload) {
  Bytes frame;
  frame.push_back(0x01);
  for (int i = 0; i < 8; ++i) {
    frame.push_back(static_cast<std::uint8_t>((offset >> (56 - 8 * i)) & 0xFF));
  }
  frame.insert(frame.end(), payload.begin(), payload.end());
  return frame;
}

std::string SubscribedMessage(std::uint64_t replay_start, std::uint64_t next,
                              bool input_available = true) {
  tmirror::Json message = tmirror::Json::Object();
  message.Set("type", tmirror::Json::String("subscribed"));
  message.Set("terminal_id", tmirror::Json::String("9ca8a5f0-1d27-4d77-af11-d40c420568d2"));
  message.Set("requested_from_offset", tmirror::Json::Uint(replay_start));
  message.Set("replay_start_offset", tmirror::Json::Uint(replay_start));
  message.Set("next_offset", tmirror::Json::Uint(next));
  message.Set("durable_offset", tmirror::Json::Uint(next));
  message.Set("earliest_offset", tmirror::Json::Uint(0));
  message.Set("terminal_state", tmirror::Json::String("open"));
  message.Set("label", tmirror::Json::String("build shell"));
  message.Set("cols", tmirror::Json::Uint(120));
  message.Set("rows", tmirror::Json::Uint(40));
  message.Set("term", tmirror::Json::String("xterm-256color"));
  message.Set("accepts_input", tmirror::Json::Bool(true));
  message.Set("input_available", tmirror::Json::Bool(input_available));
  return message.Serialize();
}

MirrorSessionConfig Config() {
  MirrorSessionConfig config;
  config.terminal_id = "9ca8a5f0-1d27-4d77-af11-d40c420568d2";
  return config;
}

}  // namespace

TM_TEST(Protocol, SubscribedEstablishesTheOffsetBaseline) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  TM_CHECK(session.HandleControlMessage(SubscribedMessage(1000, 1000)).ok());
  TM_CHECK(session.subscribed());
  TM_CHECK_EQ(session.next_expected_offset(), static_cast<std::uint64_t>(1000));
  const MirrorEvent* event = collector.Last(MirrorEventKind::kSubscribed);
  TM_REQUIRE(event != nullptr);
  TM_CHECK_EQ(event->subscribed.columns, 120u);
  TM_CHECK_EQ(event->subscribed.rows, 40u);
  TM_CHECK_EQ(event->subscribed.label, "build shell");
}

TM_TEST(Protocol, InconsistentSubscribedOffsetsAreRejected) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  // next_offset below replay_start_offset cannot be true.
  Status status = session.HandleControlMessage(SubscribedMessage(2000, 1000));
  TM_CHECK(!status.ok());
  TM_CHECK(!session.subscribed());
}

TM_TEST(Protocol, OutputIsAppliedInOrder) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(100, 100));
  TM_CHECK(session.HandleBinaryFrame(OutputFrame(100, "hello ")).ok());
  TM_CHECK(session.HandleBinaryFrame(OutputFrame(106, "world")).ok());
  TM_CHECK_EQ(collector.output, "hello world");
  TM_CHECK_EQ(session.next_expected_offset(), static_cast<std::uint64_t>(111));
}

TM_TEST(Protocol, DuplicateFramesAreNotAppliedTwice) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0));
  session.HandleBinaryFrame(OutputFrame(0, "abc"));
  // An exact repeat and a fully overlapping repeat both apply nothing.
  TM_CHECK(session.HandleBinaryFrame(OutputFrame(0, "abc")).ok());
  TM_CHECK(session.HandleBinaryFrame(OutputFrame(1, "b")).ok());
  TM_CHECK_EQ(collector.output, "abc");
  TM_CHECK_EQ(session.next_expected_offset(), static_cast<std::uint64_t>(3));
}

TM_TEST(Protocol, PartiallyOverlappingFrameAppliesOnlyTheNewBytes) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0));
  session.HandleBinaryFrame(OutputFrame(0, "abc"));
  TM_CHECK(session.HandleBinaryFrame(OutputFrame(1, "bcdef")).ok());
  TM_CHECK_EQ(collector.output, "abcdef");
  TM_CHECK_EQ(session.next_expected_offset(), static_cast<std::uint64_t>(6));
}

TM_TEST(Protocol, ForwardJumpIsReportedRatherThanRendered) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0));
  session.HandleBinaryFrame(OutputFrame(0, "abc"));
  // The relay guarantees contiguity, so a jump means the stream is untrustworthy.
  Status status = session.HandleBinaryFrame(OutputFrame(10, "xyz"));
  TM_CHECK(!status.ok());
  TM_CHECK(status.kind() == ErrorKind::kSyncFailure);
  TM_CHECK_EQ(collector.output, "abc");
}

TM_TEST(Protocol, GapMovesTheBaselineAndIsReported) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0));
  session.HandleBinaryFrame(OutputFrame(0, "old"));

  tmirror::Json gap = tmirror::Json::Object();
  gap.Set("type", tmirror::Json::String("gap"));
  gap.Set("requested_from_offset", tmirror::Json::Uint(3));
  gap.Set("available_from_offset", tmirror::Json::Uint(5000));
  TM_CHECK(session.HandleControlMessage(gap.Serialize()).ok());

  const MirrorEvent* event = collector.Last(MirrorEventKind::kGap);
  TM_REQUIRE(event != nullptr);
  TM_CHECK_EQ(event->available_from_offset, static_cast<std::uint64_t>(5000));
  // Replay continues from the available offset without a further error.
  TM_CHECK(session.HandleBinaryFrame(OutputFrame(5000, "new")).ok());
  TM_CHECK_EQ(collector.output, "oldnew");
}

TM_TEST(Protocol, MalformedFramesAreRejected) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0));

  TM_CHECK(!session.HandleBinaryFrame(Bytes()).ok());                 // empty
  TM_CHECK(!session.HandleBinaryFrame(Bytes{0x01, 0x00}).ok());       // truncated header
  TM_CHECK(!session.HandleBinaryFrame(OutputFrame(0, "")).ok());      // zero-length payload
  Bytes unknown = OutputFrame(0, "x");
  unknown[0] = 0x7F;
  TM_CHECK(!session.HandleBinaryFrame(unknown).ok());                 // unknown frame type
}

TM_TEST(Protocol, DurableOffsetOnlyMovesForward) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0));
  session.HandleControlMessage(R"({"type":"durable","durable_offset":500})");
  TM_CHECK_EQ(session.durable_offset(), static_cast<std::uint64_t>(500));
  session.HandleControlMessage(R"({"type":"durable","durable_offset":200})");
  TM_CHECK_EQ(session.durable_offset(), static_cast<std::uint64_t>(500));
}

TM_TEST(Protocol, ResizeAndCloseAreNormalised) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0));
  session.HandleControlMessage(
      R"({"type":"terminal.resize","terminal_id":"t","cols":100,"rows":30})");
  const MirrorEvent* resize = collector.Last(MirrorEventKind::kResize);
  TM_REQUIRE(resize != nullptr);
  TM_CHECK_EQ(resize->columns, 100u);
  TM_CHECK_EQ(resize->rows, 30u);

  session.HandleControlMessage(
      R"({"type":"terminal.closed","terminal_id":"t","reason":"process_exited",
          "next_offset":10,"durable_offset":10})");
  const MirrorEvent* closed = collector.Last(MirrorEventKind::kTerminalClosed);
  TM_REQUIRE(closed != nullptr);
  TM_CHECK_EQ(closed->code, "process_exited");
}

TM_TEST(Protocol, AbsurdDimensionsAreIgnored) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0));
  int before = collector.Count(MirrorEventKind::kResize);
  session.HandleControlMessage(
      R"({"type":"terminal.resize","terminal_id":"t","cols":4000000000,"rows":30})");
  TM_CHECK_EQ(collector.Count(MirrorEventKind::kResize), before);
}

TM_TEST(Protocol, ReadyLimitsAreAdopted) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(
      R"({"type":"ready","connection_id":"c","protocol":"terminal-relay.mirror.v2",
          "limits":{"max_output_frame_bytes":1000,"max_control_message_bytes":2000,
                    "max_input_frame_bytes":64,"replay_capacity_bytes":1500000,
                    "heartbeat_interval_seconds":5,"heartbeat_timeout_seconds":15},
          "settings_revision":3})");
  TM_CHECK_EQ(session.limits().max_input_frame_bytes, static_cast<std::uint64_t>(64));
  TM_CHECK_EQ(session.limits().heartbeat_timeout_seconds, static_cast<std::uint64_t>(15));
  // The replay capacity can never exceed the specification's hard maximum.
  TM_CHECK(session.limits().replay_capacity_bytes <= 1500000);
}

TM_TEST(Protocol, UnknownControlTypeIsFatalUnlessMarkedOptional) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  Status fatal = session.HandleControlMessage(R"({"type":"terminal.teleport"})");
  TM_CHECK(!fatal.ok());
  TM_CHECK(fatal.kind() == ErrorKind::kProtocolIncompatible);

  Status ignorable = session.HandleControlMessage(R"({"type":"terminal.hint","optional":true})");
  TM_CHECK(ignorable.ok());
}

TM_TEST(Protocol, ErrorMessagesMapToUserVisibleKinds) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0));

  // An input refusal leaves the subscription open (relay spec §6.3).
  Status transient = session.HandleControlMessage(
      R"({"type":"error","code":"input_undeliverable","message":"no publisher"})");
  TM_CHECK(transient.ok());
  const MirrorEvent* event = collector.Last(MirrorEventKind::kError);
  TM_REQUIRE(event != nullptr);
  TM_CHECK(event->status.kind() == ErrorKind::kInputUndeliverable);

  Status fatal = session.HandleControlMessage(
      R"({"type":"error","code":"slow_consumer","message":"too slow"})");
  TM_CHECK(!fatal.ok());
  TM_CHECK(fatal.kind() == ErrorKind::kSyncFailure);
}

TM_TEST(Protocol, InputAcknowledgementAdvancesTheSequence) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0));
  session.HandleControlMessage(R"({"type":"input.ack","accepted_through":3,"relay_sequence":9})");
  TM_CHECK_EQ(session.accepted_through(), static_cast<std::uint64_t>(3));
  TM_CHECK_EQ(session.unacknowledged_input_bytes(), static_cast<std::uint64_t>(0));
}

TM_TEST(Protocol, MalformedControlMessagesAreRejectedNotIgnored) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  TM_CHECK(!session.HandleControlMessage("not json").ok());
  TM_CHECK(!session.HandleControlMessage("[]").ok());
  TM_CHECK(!session.HandleControlMessage(R"({"no":"type"})").ok());
}

TM_TEST(Protocol, ReadOnlySubscriptionRefusesInputLocally) {
  Collector collector;
  MirrorSession session(Config(), collector.handler());
  session.HandleControlMessage(SubscribedMessage(0, 0, /*input_available=*/false));
  TM_CHECK(!session.input_available());
  tmirror::Result<std::uint64_t> sent = session.SendInput(ByteView(std::string("x")));
  TM_CHECK(!sent.ok());
  TM_CHECK(sent.status().kind() == ErrorKind::kInputRefused);
}

TM_TEST(Protocol, MirrorPathIsStable) {
  TM_CHECK_EQ(MirrorSession::MirrorPath("9ca8a5f0-1d27-4d77-af11-d40c420568d2"),
              "/v1/terminals/9ca8a5f0-1d27-4d77-af11-d40c420568d2/mirror");
}
