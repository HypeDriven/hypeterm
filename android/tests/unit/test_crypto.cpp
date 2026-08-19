// Proof-of-possession primitives: key fingerprints, signing-input verification and
// Ed25519 signatures (relay spec §3.1, §4.2; client spec §12).

#include <string>

#include "framework.h"
#include "tm/api/credentials.h"
#include "tm/crypto/crypto.h"
#include "tm/crypto/identity.h"
#include "tm/util/base64.h"
#include "tm/util/strings.h"

using tmirror::Base64UrlDecode;
using tmirror::Bytes;
using tmirror::ByteView;
using tmirror::Result;
using tmirror::Status;
using tmirror::api::CredentialStore;
using tmirror::api::DeviceCredentials;
using tmirror::api::InMemorySecureStore;
using tmirror::crypto::ChallengeOperation;
using tmirror::crypto::Ed25519KeyPair;
using tmirror::crypto::Ed25519Verify;
using tmirror::crypto::ExpectedSigningInput;
using tmirror::crypto::KeyFingerprint;
using tmirror::crypto::kAlgorithmEd25519;
using tmirror::crypto::LengthPrefixed;
using tmirror::crypto::Sha256;
using tmirror::crypto::SigningInput;
using tmirror::crypto::VerifySigningInput;

TM_TEST(Crypto, Sha256MatchesKnownVector) {
  Bytes digest = Sha256(ByteView(std::string("abc")));
  TM_CHECK_EQ(tmirror::HexEncode(ByteView(digest)),
              "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}

TM_TEST(Crypto, Ed25519SignAndVerify) {
  Result<Ed25519KeyPair> key = Ed25519KeyPair::Generate();
  TM_REQUIRE(key.ok());
  TM_CHECK_EQ(key.value().public_key().size(), static_cast<std::size_t>(32));

  std::string message = "terminal-relay-challenge-v1 payload";
  Result<Bytes> signature = key.value().Sign(ByteView(message));
  TM_REQUIRE(signature.ok());
  TM_CHECK_EQ(signature.value().size(), static_cast<std::size_t>(64));
  TM_CHECK(Ed25519Verify(ByteView(key.value().public_key()), ByteView(message),
                         ByteView(signature.value())));

  std::string tampered = message + "!";
  TM_CHECK(!Ed25519Verify(ByteView(key.value().public_key()), ByteView(tampered),
                          ByteView(signature.value())));
}

TM_TEST(Crypto, KeyPairIsDeterministicFromItsSeed) {
  Bytes seed(32, 0x42);
  Result<Ed25519KeyPair> first = Ed25519KeyPair::FromSeed(ByteView(seed));
  Result<Ed25519KeyPair> second = Ed25519KeyPair::FromSeed(ByteView(seed));
  TM_REQUIRE(first.ok() && second.ok());
  TM_CHECK(first.value().public_key() == second.value().public_key());

  Bytes wrong(31, 0x42);
  TM_CHECK(!Ed25519KeyPair::FromSeed(ByteView(wrong)).ok());
}

TM_TEST(Crypto, LengthPrefixedEncodingRoundTrips) {
  LengthPrefixed encoder;
  encoder.FieldString("context").FieldString("").FieldUint64(1234);
  std::vector<Bytes> fields;
  TM_REQUIRE(LengthPrefixed::Split(ByteView(encoder.Finish()), &fields));
  TM_CHECK_EQ(fields.size(), static_cast<std::size_t>(3));
  TM_CHECK_EQ(tmirror::StringFromBytes(fields[0]), "context");
  TM_CHECK(fields[1].empty());
  TM_CHECK_EQ(fields[2].size(), static_cast<std::size_t>(8));
}

TM_TEST(Crypto, LengthPrefixedSplitRejectsTruncatedInput) {
  Bytes truncated = {0x00, 0x00, 0x00, 0x08, 0x01, 0x02};
  std::vector<Bytes> fields;
  TM_CHECK(!LengthPrefixed::Split(ByteView(truncated), &fields));
}

TM_TEST(Crypto, FingerprintIsStableAndDomainSeparated) {
  Bytes key(32, 0x01);
  std::string fingerprint = KeyFingerprint(kAlgorithmEd25519, ByteView(key));
  TM_CHECK(!fingerprint.empty());
  TM_CHECK(fingerprint.find('=') == std::string::npos);
  // The same key always yields the same identity ID (relay spec §3.1).
  TM_CHECK_EQ(fingerprint, KeyFingerprint("ED25519", ByteView(key)));
  Bytes other(32, 0x02);
  TM_CHECK_NE(fingerprint, KeyFingerprint(kAlgorithmEd25519, ByteView(other)));

  // And it is exactly base64url(sha256(lp(context)||lp(alg)||lp(key))).
  LengthPrefixed encoder;
  encoder.FieldString("terminal-relay-identity-v1").FieldString("ed25519").Field(ByteView(key));
  TM_CHECK_EQ(fingerprint,
              tmirror::Base64UrlEncode(ByteView(Sha256(ByteView(encoder.Finish())))));
}

TM_TEST(Crypto, SigningInputVerificationAcceptsAMatchingChallenge) {
  SigningInput input;
  input.origin = "https://relay.example";
  input.challenge_id = "01K";
  input.challenge = Bytes(32, 0x7F);
  input.operation = ChallengeOperation::kAuthenticateDevice;
  input.key_fingerprint = "fp";
  input.expires_at_unix_ms = 1700000000000ULL;

  ExpectedSigningInput expected;
  expected.challenge_id = "01K";
  expected.challenge = input.challenge;
  expected.operation = ChallengeOperation::kAuthenticateDevice;
  expected.key_fingerprint = "fp";
  expected.expected_origin = "https://relay.example";

  TM_CHECK(VerifySigningInput(ByteView(input.Encode()), expected).ok());
}

TM_TEST(Crypto, SigningInputVerificationRefusesSubstitutions) {
  SigningInput input;
  input.origin = "https://relay.example";
  input.challenge_id = "01K";
  input.challenge = Bytes(32, 0x7F);
  input.operation = ChallengeOperation::kAuthenticateDevice;
  input.key_fingerprint = "fp";

  ExpectedSigningInput expected;
  expected.challenge_id = "01K";
  expected.challenge = input.challenge;
  expected.operation = ChallengeOperation::kAuthenticateDevice;
  expected.key_fingerprint = "fp";

  // A relay that swaps the operation, the challenge or the key is not getting a
  // signature over bytes of its choosing (spec §12).
  SigningInput swapped_operation = input;
  swapped_operation.operation = ChallengeOperation::kRegisterDevice;
  TM_CHECK(!VerifySigningInput(ByteView(swapped_operation.Encode()), expected).ok());

  SigningInput swapped_challenge = input;
  swapped_challenge.challenge = Bytes(32, 0x01);
  TM_CHECK(!VerifySigningInput(ByteView(swapped_challenge.Encode()), expected).ok());

  SigningInput swapped_key = input;
  swapped_key.key_fingerprint = "other";
  TM_CHECK(!VerifySigningInput(ByteView(swapped_key.Encode()), expected).ok());

  SigningInput swapped_id = input;
  swapped_id.challenge_id = "02K";
  TM_CHECK(!VerifySigningInput(ByteView(swapped_id.Encode()), expected).ok());

  // Nine fields exactly; anything else is not the encoding we agreed on.
  LengthPrefixed short_encoding;
  short_encoding.FieldString("terminal-relay-challenge-v1");
  TM_CHECK(!VerifySigningInput(ByteView(short_encoding.Finish()), expected).ok());
}

TM_TEST(Crypto, SigningInputOriginIsOptional) {
  SigningInput input;
  input.origin = "https://proxy.example";
  input.challenge_id = "01K";
  input.challenge = Bytes(32, 0x7F);
  input.operation = ChallengeOperation::kAuthenticateIdentity;
  input.key_fingerprint = "fp";

  ExpectedSigningInput expected;
  expected.challenge_id = "01K";
  expected.challenge = input.challenge;
  expected.operation = ChallengeOperation::kAuthenticateIdentity;
  expected.key_fingerprint = "fp";
  // A deployment behind a proxy may legitimately bind a different origin.
  TM_CHECK(VerifySigningInput(ByteView(input.Encode()), expected).ok());
  expected.expected_origin = "https://relay.example";
  TM_CHECK(!VerifySigningInput(ByteView(input.Encode()), expected).ok());
}

TM_TEST(Credentials, GenerateSaveAndLoadRoundTrip) {
  InMemorySecureStore store;
  CredentialStore credentials(&store);
  TM_CHECK(!credentials.HasCredentials());

  Result<DeviceCredentials> generated =
      CredentialStore::GenerateNew("https://relay.example", "Pixel");
  TM_REQUIRE(generated.ok());
  generated.value().identity_id = "identity";
  generated.value().device_id = "device";
  TM_CHECK(generated.value().complete());
  TM_CHECK(credentials.Save(generated.value()).ok());
  TM_CHECK(credentials.HasCredentials());

  Result<DeviceCredentials> loaded = credentials.Load();
  TM_REQUIRE(loaded.ok());
  TM_CHECK_EQ(loaded.value().identity_id, "identity");
  TM_CHECK_EQ(loaded.value().device_id, "device");
  TM_CHECK_EQ(loaded.value().server_url, "https://relay.example");
  TM_CHECK(loaded.value().private_key_seed == generated.value().private_key_seed);

  Result<Ed25519KeyPair> key = loaded.value().LoadKeyPair();
  TM_REQUIRE(key.ok());
  TM_CHECK_EQ(tmirror::Base64UrlEncode(ByteView(key.value().public_key())),
              generated.value().public_key_base64url);

  TM_CHECK(credentials.Clear().ok());
  TM_CHECK(!credentials.HasCredentials());
}

TM_TEST(Credentials, RefusesToSaveAKeylessCredential) {
  InMemorySecureStore store;
  CredentialStore credentials(&store);
  DeviceCredentials empty;
  empty.server_url = "https://relay.example";
  TM_CHECK(!credentials.Save(empty).ok());
}

TM_TEST(Credentials, CorruptStorageIsReportedNotCrashed) {
  InMemorySecureStore store;
  std::string garbage = "not json";
  store.Put("device_credentials_v1", ByteView(garbage));
  CredentialStore credentials(&store);
  Result<DeviceCredentials> loaded = credentials.Load();
  TM_CHECK(!loaded.ok());
  TM_CHECK(loaded.status().kind() == tmirror::ErrorKind::kStorageError);
}
