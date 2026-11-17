package com.mimint.shell

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.math.BigInteger
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.MessageDigest
import java.security.SecureRandom
import java.security.cert.Certificate
import java.util.Base64
import java.util.Calendar
import javax.security.auth.x500.X500Principal

/**
 * The shell's identity: a self-signed TLS certificate + the
 * pairing token. Both are generated on first run, on the
 * device, and never leave it except through the pairing UI
 * (the user reads them and enters them on the Rust side).
 *
 * - The certificate lives in AndroidKeyStore under [ALIAS].
 *   Keystore generates a self-signed certificate automatically
 *   when the key pair is created with a certificate subject.
 *   The private key never leaves the keystore.
 * - The pairing token is 32 random bytes, base64url, stored in
 *   an app-private file. There is no default token.
 *
 * The Rust client pins the SHA-256 of the certificate DER
 * (see `fingerprintHex`). The pin *is* the identity: no CA,
 * no hostname check, no trust store.
 */
object IdentityStore {
    private const val ALIAS = "mimint-shell-tls"
    private const val TOKEN_FILE = "pairing-token.txt"

    /** The TLS certificate chain for the SSL server socket. */
    fun certificateChain(): Array<Certificate> {
        val ks = loadKeyStore()
        if (!ks.containsAlias(ALIAS)) generateKeyPair()
        return ks.getCertificateChain(ALIAS)
            ?: throw IllegalStateException("no certificate chain for $ALIAS")
    }

    /** SHA-256 of the certificate DER, lowercase hex, no colons. */
    fun fingerprintHex(): String {
        val cert = certificateChain().first()
        val digest = MessageDigest.getInstance("SHA-256").digest(cert.encoded)
        return digest.joinToString("") { "%02x".format(it) }
    }

    /** The fingerprint as shown in the pairing UI (colon groups). */
    fun fingerprintDisplay(): String =
        fingerprintHex().chunked(2).joinToString(":")

    private fun loadKeyStore(): KeyStore {
        val ks = KeyStore.getInstance("AndroidKeyStore")
        ks.load(null)
        return ks
    }

    private fun generateKeyPair() {
        val now = Calendar.getInstance()
        val notBefore = now.time
        now.add(Calendar.YEAR, 10)
        val notAfter = now.time
        val spec = KeyGenParameterSpec.Builder(
            ALIAS,
            KeyProperties.PURPOSE_SIGN or KeyProperties.PURPOSE_VERIFY,
        )
            .setKeySize(2048)
            .setDigests(KeyProperties.DIGEST_SHA256)
            .setSignaturePaddings(KeyProperties.SIGNATURE_PADDING_RSA_PKCS1)
            .setCertificateSubject(X500Principal("CN=127.0.0.1"))
            .setCertificateSerialNumber(BigInteger.valueOf(SecureRandom().nextLong()))
            .setCertificateNotBefore(notBefore)
            .setCertificateNotAfter(notAfter)
            .build()
        KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_RSA, "AndroidKeyStore")
            .apply { initialize(spec) }
            .generateKeyPair()
    }

    /** The pairing token, generating and persisting it on first run. */
    fun pairingToken(context: Context): String {
        val file = File(context.filesDir, TOKEN_FILE)
        if (file.exists()) {
            val stored = file.readText().trim()
            if (stored.isNotEmpty()) return stored
        }
        val bytes = ByteArray(32).also { SecureRandom().nextBytes(it) }
        val token = Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)
        file.writeText(token)
        return token
    }
}
