package com.hypedriven.hypeterm

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Keystore-backed storage for the device's private key (spec §12).
 *
 * The Ed25519 key itself cannot live inside the Keystore on the minimum supported
 * platform — hardware Ed25519 only arrives later — so instead an AES-256-GCM key is
 * generated *in* the Keystore, never leaves it, and seals the Ed25519 seed before it
 * is written to disk. Extracting the seed then requires code execution on the device
 * with the app's identity, which is the property the specification is after.
 *
 * StrongBox is requested when the device has it and the request is retried without it
 * otherwise, because asking for StrongBox on a device without it throws rather than
 * degrading.
 */
class KeystoreSecureStore(context: Context) {

    private val preferences =
        context.getSharedPreferences(PREFERENCES_NAME, Context.MODE_PRIVATE)

    /** Called from native code. Returns false when sealing fails. */
    fun put(key: String, value: ByteArray): Boolean {
        return try {
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.ENCRYPT_MODE, secretKey())
            val sealed = cipher.doFinal(value)
            val payload = cipher.iv + sealed
            preferences.edit()
                .putString(key, Base64.encodeToString(payload, Base64.NO_WRAP))
                .commit()
        } catch (error: Exception) {
            false
        }
    }

    /** Called from native code. Returns null when absent or unreadable. */
    fun get(key: String): ByteArray? {
        val stored = preferences.getString(key, null) ?: return null
        return try {
            val payload = Base64.decode(stored, Base64.NO_WRAP)
            if (payload.size <= IV_BYTES) return null
            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(
                Cipher.DECRYPT_MODE,
                secretKey(),
                GCMParameterSpec(TAG_BITS, payload, 0, IV_BYTES),
            )
            cipher.doFinal(payload, IV_BYTES, payload.size - IV_BYTES)
        } catch (error: Exception) {
            // A key that no longer decrypts (device reset, key invalidated) is gone;
            // the app must re-pair rather than pretend it has a credential.
            preferences.edit().remove(key).apply()
            null
        }
    }

    fun remove(key: String): Boolean = preferences.edit().remove(key).commit()

    fun contains(key: String): Boolean = preferences.contains(key)

    private fun secretKey(): SecretKey {
        val keyStore = KeyStore.getInstance(ANDROID_KEYSTORE).apply { load(null) }
        val existing = keyStore.getKey(KEY_ALIAS, null) as? SecretKey
        if (existing != null) return existing
        return generateKey(strongBox = true) ?: generateKey(strongBox = false)
            ?: throw IllegalStateException("cannot create a Keystore key")
    }

    private fun generateKey(strongBox: Boolean): SecretKey? {
        return try {
            val generator =
                KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, ANDROID_KEYSTORE)
            val builder = KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                // Deliberately not user-authentication-bound: the client reconnects in
                // the background, and a locked screen must not lose the session.
                .setRandomizedEncryptionRequired(true)
            if (strongBox && android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.P) {
                builder.setIsStrongBoxBacked(true)
            }
            generator.init(builder.build())
            generator.generateKey()
        } catch (error: Exception) {
            null
        }
    }

    private companion object {
        const val ANDROID_KEYSTORE = "AndroidKeyStore"
        const val KEY_ALIAS = "hypeterm-credential-key"
        const val TRANSFORMATION = "AES/GCM/NoPadding"
        const val PREFERENCES_NAME = "hypeterm_secure"
        const val IV_BYTES = 12
        const val TAG_BITS = 128
    }
}
