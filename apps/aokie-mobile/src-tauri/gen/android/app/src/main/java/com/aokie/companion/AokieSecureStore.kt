package com.aokie.companion

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyPermanentlyInvalidatedException
import android.security.keystore.KeyProperties
import android.util.AtomicFile
import android.util.Log
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.DataOutputStream
import java.security.KeyStore
import java.security.MessageDigest
import java.security.UnrecoverableKeyException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * Small native-only encrypted store used by Rust through MainActivity.
 *
 * The non-exportable AES key lives in Android Keystore. Ciphertext lives in
 * noBackupFilesDir, never SharedPreferences, WebView storage, logs, or Android
 * backup. Each logical key is authenticated as AES-GCM additional data so a
 * file cannot be substituted for another account.
 */
object AokieSecureStore {
  private const val KEY_ALIAS = "com.aokie.companion.native-store.v1"
  private const val STORE_DIRECTORY = "aokie-native-store-v1"
  private const val MAX_KEY_BYTES = 512
  private const val MAX_VALUE_BYTES = 64 * 1024
  private const val TAG = "AokieSecureStore"
  private val magic = byteArrayOf(0x41, 0x4f, 0x4b, 0x31) // AOK1
  private val lock = Any()

  fun isAvailable(context: Context): Boolean = synchronized(lock) {
    runCatching {
      key(context)
      true
    }.getOrDefault(false)
  }

  fun put(context: Context, logicalKey: String, value: String): Boolean = synchronized(lock) {
    runCatching {
      val keyBytes = checkedKey(logicalKey)
      val valueBytes = value.toByteArray(Charsets.UTF_8)
      require(valueBytes.size <= MAX_VALUE_BYTES) { "secure value is too large" }

      val cipher = Cipher.getInstance("AES/GCM/NoPadding")
      try {
        cipher.init(Cipher.ENCRYPT_MODE, key(context))
      } catch (error: KeyPermanentlyInvalidatedException) {
        // A definite loss: reset the store, and this write goes under a fresh key.
        resetLostKey(context)
        cipher.init(Cipher.ENCRYPT_MODE, key(context))
      }
      cipher.updateAAD(keyBytes)
      val ciphertext = cipher.doFinal(valueBytes)
      val encoded = ByteArrayOutputStream().use { bytes ->
        DataOutputStream(bytes).use { output ->
          output.write(magic)
          output.writeByte(cipher.iv.size)
          output.write(cipher.iv)
          output.writeInt(ciphertext.size)
          output.write(ciphertext)
        }
        bytes.toByteArray()
      }

      val atomic = AtomicFile(file(context, logicalKey))
      val stream = atomic.startWrite()
      try {
        stream.write(encoded)
        stream.fd.sync()
        atomic.finishWrite(stream)
      } catch (error: Throwable) {
        atomic.failWrite(stream)
        throw error
      }
      true
    }.getOrDefault(false)
  }

  /**
   * For Kotlin's own short-lived records (offers, call actions, the push token), where an unreadable value is
   * as good as none. Rust's identity, session and profile records go through [read] instead: there a read
   * error must never look like absence, or the caller would mint a new identity or sign the user out.
   */
  fun get(context: Context, logicalKey: String): String? = read(context, logicalKey).getOrNull()

  /**
   * The stored value, `null` only when nothing is stored under [logicalKey], or a failure for any key, file,
   * format, Keystore or decryption error.
   */
  fun read(context: Context, logicalKey: String): Result<String?> = synchronized(lock) {
    runCatching {
      val keyBytes = checkedKey(logicalKey)
      val target = file(context, logicalKey)
      // The key first: if it was lost, key() clears every value it sealed, and this one reads as absent.
      val secret = key(context)
      if (!storedFileExists(target)) return@runCatching null
      val encoded = AtomicFile(target).readFully()
      require(encoded.size <= MAX_VALUE_BYTES + 128) { "secure value is too large" }

      val (iv, ciphertext) = DataInputStream(ByteArrayInputStream(encoded)).use { input ->
        val actualMagic = ByteArray(magic.size)
        input.readFully(actualMagic)
        require(actualMagic.contentEquals(magic)) { "secure value version is invalid" }
        val ivSize = input.readUnsignedByte()
        require(ivSize in 12..32) { "secure value IV is invalid" }
        val iv = ByteArray(ivSize)
        input.readFully(iv)
        val ciphertextSize = input.readInt()
        require(ciphertextSize in 16..(MAX_VALUE_BYTES + 32)) { "secure value is invalid" }
        val ciphertext = ByteArray(ciphertextSize)
        input.readFully(ciphertext)
        require(input.read() == -1) { "secure value has trailing bytes" }
        iv to ciphertext
      }

      val cipher = Cipher.getInstance("AES/GCM/NoPadding")
      try {
        cipher.init(Cipher.DECRYPT_MODE, secret, GCMParameterSpec(128, iv))
      } catch (error: KeyPermanentlyInvalidatedException) {
        // The value can never be read again; after the reset it is truly absent.
        resetLostKey(context)
        return@runCatching null
      }
      cipher.updateAAD(keyBytes)
      val plaintext = cipher.doFinal(ciphertext)
      require(plaintext.size <= MAX_VALUE_BYTES) { "secure value is too large" }
      plaintext.toString(Charsets.UTF_8)
    }
  }

  fun delete(context: Context, logicalKey: String): Boolean = synchronized(lock) {
    runCatching {
      checkedKey(logicalKey)
      AtomicFile(file(context, logicalKey)).delete()
      true
    }.getOrDefault(false)
  }

  fun contains(context: Context, logicalKey: String): Boolean = synchronized(lock) {
    runCatching {
      checkedKey(logicalKey)
      storedFileExists(file(context, logicalKey))
    }.getOrDefault(false)
  }

  /**
   * Whether a value is stored in [target], counting the backup AtomicFile keeps mid-write.
   *
   * Before API 30, AtomicFile writes by renaming the old file to `<name>.bak` and restores it on the next
   * read. A process killed mid-write therefore left only the backup, and a plain `exists()` check read the
   * value as absent: Rust would then mint a new identity or treat the person as signed out.
   */
  private fun storedFileExists(target: java.io.File): Boolean =
    target.exists() || java.io.File(target.path + ".bak").exists()

  private fun key(context: Context): SecretKey {
    val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    val existing = try {
      store.getKey(KEY_ALIAS, null) as? SecretKey
    } catch (error: UnrecoverableKeyException) {
      // The key's material is gone for good (some devices lose it across an OS update or a lock-screen
      // change). Other Keystore errors stay errors: they can be transient, and treating one as loss would
      // throw away a readable identity.
      Log.w(TAG, "the native store key is unrecoverable; resetting the store")
      runCatching { store.deleteEntry(KEY_ALIAS) }
      null
    }
    existing?.let { return it }
    // No key: every value in the store was sealed by a key that no longer exists and can never be read
    // again. Clear them, so they read as absent (a fresh identity, a new sign-in) instead of failing on
    // every launch with no way out. On a first launch there is nothing to clear.
    resetUnreadableValues(context)
    val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
    generator.init(
      KeyGenParameterSpec.Builder(
        KEY_ALIAS,
        KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
      )
        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
        .setKeySize(256)
        .setRandomizedEncryptionRequired(true)
        .build(),
    )
    return generator.generateKey()
  }

  /** Forget a key Android reports permanently invalid, and every value it sealed. */
  private fun resetLostKey(context: Context) {
    Log.w(TAG, "the native store key was permanently invalidated; resetting the store")
    runCatching {
      KeyStore.getInstance("AndroidKeyStore").apply { load(null) }.deleteEntry(KEY_ALIAS)
    }
    resetUnreadableValues(context)
  }

  private fun resetUnreadableValues(context: Context) {
    val directory = java.io.File(context.noBackupFilesDir, STORE_DIRECTORY)
    val stale = directory.listFiles().orEmpty()
    if (stale.isEmpty()) return
    Log.w(TAG, "clearing ${stale.size} values sealed by a lost key")
    stale.forEach { runCatching { it.delete() } }
  }

  private fun checkedKey(logicalKey: String): ByteArray {
    val bytes = logicalKey.toByteArray(Charsets.UTF_8)
    require(bytes.isNotEmpty() && bytes.size <= MAX_KEY_BYTES) { "secure key is invalid" }
    require(logicalKey.none { it.isISOControl() }) { "secure key is invalid" }
    return bytes
  }

  private fun file(context: Context, logicalKey: String): java.io.File {
    val directory = java.io.File(context.noBackupFilesDir, STORE_DIRECTORY)
    require(directory.exists() || directory.mkdirs()) { "secure store directory is unavailable" }
    val digest = MessageDigest.getInstance("SHA-256").digest(logicalKey.toByteArray(Charsets.UTF_8))
    val name = digest.joinToString(separator = "") { byte -> "%02x".format(byte.toInt() and 0xff) }
    return java.io.File(directory, "$name.bin")
  }
}
