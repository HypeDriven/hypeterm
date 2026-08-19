#include "tm/crypto/crypto.h"

#include <openssl/crypto.h>
#include <openssl/evp.h>

#include <utility>

#include "tm/util/random.h"

namespace tmirror {
namespace crypto {
namespace {

struct PkeyDeleter {
  void operator()(EVP_PKEY* p) const { EVP_PKEY_free(p); }
};
using PkeyPtr = std::unique_ptr<EVP_PKEY, PkeyDeleter>;

struct MdCtxDeleter {
  void operator()(EVP_MD_CTX* c) const { EVP_MD_CTX_free(c); }
};
using MdCtxPtr = std::unique_ptr<EVP_MD_CTX, MdCtxDeleter>;

Status CryptoError(const std::string& what) {
  return Status::Error(ErrorKind::kInternal, "crypto: " + what);
}

}  // namespace

Bytes Sha256(ByteView data) {
  Bytes digest(32);
  unsigned int length = 0;
  if (EVP_Digest(data.data(), data.size(), digest.data(), &length, EVP_sha256(), nullptr) != 1) {
    return Bytes();
  }
  digest.resize(length);
  return digest;
}

Bytes Sha1(ByteView data) {
  Bytes digest(20);
  unsigned int length = 0;
  if (EVP_Digest(data.data(), data.size(), digest.data(), &length, EVP_sha1(), nullptr) != 1) {
    return Bytes();
  }
  digest.resize(length);
  return digest;
}

bool ConstantTimeEquals(ByteView a, ByteView b) {
  if (a.size() != b.size()) return false;
  return CRYPTO_memcmp(a.data(), b.data(), a.size()) == 0;
}

Ed25519KeyPair::~Ed25519KeyPair() { SecureZero(seed_); }

Ed25519KeyPair::Ed25519KeyPair(Ed25519KeyPair&& other) noexcept
    : seed_(std::move(other.seed_)), public_key_(std::move(other.public_key_)) {
  other.seed_.clear();
}

Ed25519KeyPair& Ed25519KeyPair::operator=(Ed25519KeyPair&& other) noexcept {
  if (this != &other) {
    SecureZero(seed_);
    seed_ = std::move(other.seed_);
    public_key_ = std::move(other.public_key_);
    other.seed_.clear();
  }
  return *this;
}

Result<Ed25519KeyPair> Ed25519KeyPair::Generate() {
  Bytes seed(kSeedSize);
  if (!SecureRandomBytes(seed.data(), seed.size())) {
    return CryptoError("secure random source unavailable");
  }
  Result<Ed25519KeyPair> pair = FromSeed(ByteView(seed));
  SecureZero(seed);
  return pair;
}

Result<Ed25519KeyPair> Ed25519KeyPair::FromSeed(ByteView seed) {
  if (seed.size() != kSeedSize) {
    return Status::Error(ErrorKind::kInvalidArgument, "ed25519 seed must be 32 bytes");
  }
  PkeyPtr key(EVP_PKEY_new_raw_private_key(EVP_PKEY_ED25519, nullptr, seed.data(), seed.size()));
  if (!key) return CryptoError("could not construct an ed25519 key from the seed");

  Ed25519KeyPair pair;
  pair.seed_.assign(seed.begin(), seed.end());
  pair.public_key_.resize(kPublicKeySize);
  std::size_t length = pair.public_key_.size();
  if (EVP_PKEY_get_raw_public_key(key.get(), pair.public_key_.data(), &length) != 1 ||
      length != kPublicKeySize) {
    SecureZero(pair.seed_);
    return CryptoError("could not derive the ed25519 public key");
  }
  return pair;
}

Result<Bytes> Ed25519KeyPair::Sign(ByteView message) const {
  if (!valid()) return Status::Error(ErrorKind::kInvalidArgument, "signing key is not loaded");
  PkeyPtr key(
      EVP_PKEY_new_raw_private_key(EVP_PKEY_ED25519, nullptr, seed_.data(), seed_.size()));
  if (!key) return CryptoError("could not load the signing key");

  MdCtxPtr ctx(EVP_MD_CTX_new());
  if (!ctx) return CryptoError("could not allocate a signing context");
  if (EVP_DigestSignInit(ctx.get(), nullptr, nullptr, nullptr, key.get()) != 1) {
    return CryptoError("could not initialise ed25519 signing");
  }
  std::size_t signature_length = 0;
  if (EVP_DigestSign(ctx.get(), nullptr, &signature_length, message.data(), message.size()) != 1) {
    return CryptoError("could not size the signature");
  }
  Bytes signature(signature_length);
  if (EVP_DigestSign(ctx.get(), signature.data(), &signature_length, message.data(),
                     message.size()) != 1) {
    return CryptoError("signing failed");
  }
  signature.resize(signature_length);
  return signature;
}

bool Ed25519Verify(ByteView public_key, ByteView message, ByteView signature) {
  if (public_key.size() != Ed25519KeyPair::kPublicKeySize) return false;
  if (signature.size() != Ed25519KeyPair::kSignatureSize) return false;
  PkeyPtr key(EVP_PKEY_new_raw_public_key(EVP_PKEY_ED25519, nullptr, public_key.data(),
                                          public_key.size()));
  if (!key) return false;
  MdCtxPtr ctx(EVP_MD_CTX_new());
  if (!ctx) return false;
  if (EVP_DigestVerifyInit(ctx.get(), nullptr, nullptr, nullptr, key.get()) != 1) return false;
  return EVP_DigestVerify(ctx.get(), signature.data(), signature.size(), message.data(),
                          message.size()) == 1;
}

}  // namespace crypto
}  // namespace tmirror
