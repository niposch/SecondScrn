package dev.secondscrn.client

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import uniffi.secondscrn_client.SecretStore
import uniffi.secondscrn_client.StoreException

/**
 * Encrypts the tablet's identity (its TLS private key) with an AES-256-GCM
 * key held by the Android Keystore. The key is non-exportable and, on devices
 * with secure hardware, never leaves it.
 *
 * Blob format: 12-byte IV followed by ciphertext and tag.
 */
class KeystoreSecretStore : SecretStore {
    private val alias = "secondscrn-identity-v1"

    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(alias, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build(),
        )
        return gen.generateKey()
    }

    override fun protect(data: ByteArray): ByteArray = try {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, key())
        cipher.iv + cipher.doFinal(data)
    } catch (e: Exception) {
        throw StoreException.Failed(e.toString())
    }

    override fun unprotect(data: ByteArray): ByteArray = try {
        require(data.size > 12) { "blob too short" }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, data, 0, 12))
        cipher.doFinal(data, 12, data.size - 12)
    } catch (e: Exception) {
        throw StoreException.Failed(e.toString())
    }
}
