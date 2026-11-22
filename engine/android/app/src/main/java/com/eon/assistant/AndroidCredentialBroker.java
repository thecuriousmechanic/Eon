package com.eon.assistant;

import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyInfo;
import android.security.keystore.KeyProperties;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.Key;
import java.security.KeyStore;
import java.security.MessageDigest;
import java.util.Arrays;
import java.util.HashMap;
import java.util.Map;
import java.util.UUID;

import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.SecretKeyFactory;
import javax.crypto.spec.GCMParameterSpec;

/** App-host capability. Nothing here is an Intent, Binder, UniFFI or agent API. */
final class AndroidCredentialBroker implements AutoCloseable {
    private static final String PROVIDER = "AndroidKeyStore";
    // Exactly one reserved alias per application UID, not one per handle, broker or process start.
    static final String ALIAS = "com.eon.assistant.credentials.aes-gcm.v1";
    static final int CAPACITY = 8;
    private static final int MAX_BYTES = 32_767;
    private static final String AES = KeyProperties.KEY_ALGORITHM_AES;
    private static final int PURPOSES = KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT;
    private static final String TRANSFORM = "AES/GCM/NoPadding";
    private static final int TAG_BITS = 128;
    private static final int IV_BYTES = 12;

    // The foundation's IdentityAuthority isn't a wire protocol. As there, IDs aren't authority.
    // The host keeps the issuer; a recipient of either final grant cannot mint or amplify it.
    static final class Authority {
        Owner newOwner() { return new Owner(this); }
    }

    static final class Owner {
        final Grant manage;
        final Grant use;

        private Owner(Authority issuer) {
            UUID id = UUID.randomUUID();
            manage = new Grant(issuer, id, Right.MANAGE);
            use = new Grant(issuer, id, Right.USE);
        }
    }

    private enum Right { MANAGE, USE }

    static final class Grant {
        private final Authority issuer;
        private final UUID owner;
        private final Right right;

        private Grant(Authority issuer, UUID owner, Right right) {
            this.issuer = issuer;
            this.owner = owner;
            this.right = right;
        }
    }

    private static final class Entry {
        final UUID owner;
        final byte[] iv;
        final byte[] ciphertext;

        Entry(UUID owner, byte[] iv, byte[] ciphertext) {
            this.owner = owner;
            this.iv = iv;
            this.ciphertext = ciphertext;
        }

        void clear() {
            Arrays.fill(iv, (byte) 0);
            Arrays.fill(ciphertext, (byte) 0);
        }
    }

    private final Authority issuer;
    private final Map<String, Entry> entries = new HashMap<>();
    private boolean closed;

    AndroidCredentialBroker(Authority issuer) { this.issuer = issuer; }

    private boolean authorized(Grant grant, Right right) {
        return !closed && grant != null && grant.issuer == issuer && grant.right == right;
    }

    private Entry owned(Grant grant, Right right, String handle) {
        if (!authorized(grant, right)) return null;
        Entry entry = entries.get(handle);
        return entry != null && entry.owner.equals(grant.owner) ? entry : null;
    }

    // Serialized across brokers in this single-process app. Reads never recreate an invalidated key.
    private static synchronized SecretKey key(boolean create) throws GeneralSecurityException, IOException {
        KeyStore store = KeyStore.getInstance(PROVIDER);
        store.load(null);
        if (create && !store.containsAlias(ALIAS)) {
            KeyGenerator generator = KeyGenerator.getInstance(AES, PROVIDER);
            generator.init(new KeyGenParameterSpec.Builder(ALIAS, PURPOSES)
                    .setKeySize(256)
                    .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setRandomizedEncryptionRequired(true)
                    .build());
            generator.generateKey();
        }
        Key candidate = store.getKey(ALIAS, null);
        if (!(candidate instanceof SecretKey) || !AES.equals(candidate.getAlgorithm())) {
            throw new GeneralSecurityException();
        }
        byte[] export = candidate.getEncoded();
        if (export != null) {
            Arrays.fill(export, (byte) 0);
            throw new GeneralSecurityException();
        }
        SecretKey key = (SecretKey) candidate;
        KeyInfo info = (KeyInfo) SecretKeyFactory.getInstance(AES, PROVIDER).getKeySpec(key, KeyInfo.class);
        if (key.getFormat() != null || !ALIAS.equals(info.getKeystoreAlias())
                || info.getOrigin() != KeyProperties.ORIGIN_GENERATED || info.getKeySize() != 256
                || info.getPurposes() != PURPOSES
                || !Arrays.equals(info.getBlockModes(), new String[] {KeyProperties.BLOCK_MODE_GCM})
                || !Arrays.equals(info.getEncryptionPaddings(), new String[] {KeyProperties.ENCRYPTION_PADDING_NONE})) {
            throw new GeneralSecurityException();
        }
        return key;
    }

    private static void bindContext(Cipher cipher, Grant grant, String handle) {
        cipher.updateAAD(("eon:inspection-proof:v1:" + grant.owner + ":" + handle).getBytes(StandardCharsets.US_ASCII));
    }

    /** Takes ownership of the input buffer, including on denial or provider failure. */
    synchronized String put(Grant manager, byte[] secret) {
        if (secret == null) return null;
        try {
            if (!authorized(manager, Right.MANAGE) || entries.size() >= CAPACITY
                    || secret.length == 0 || secret.length > MAX_BYTES) return null;
            String handle = "secret://android/" + UUID.randomUUID();
            if (entries.containsKey(handle)) return null;
            Cipher cipher = Cipher.getInstance(TRANSFORM);
            cipher.init(Cipher.ENCRYPT_MODE, key(true)); // Provider-generated IV; no caller nonce.
            byte[] iv = cipher.getIV();
            if (iv == null || iv.length != IV_BYTES) return null;
            bindContext(cipher, manager, handle);
            byte[] ciphertext = cipher.doFinal(secret);
            Entry entry = new Entry(manager.owner, iv, ciphertext);
            if (ciphertext.length != secret.length + TAG_BITS / 8) {
                entry.clear();
                return null;
            }
            entries.put(handle, entry);
            return handle;
        } catch (GeneralSecurityException | IOException | RuntimeException unavailable) {
            return null;
        } finally {
            Arrays.fill(secret, (byte) 0);
        }
    }

    /** Fixed local sink: compare a one-time approval nonce against the host's commitment.
     * No byte getter, user-supplied callback, output stream, child process or signing oracle. */
    synchronized boolean consumeInspectionProof(Grant user, String handle, byte[] commitment) {
        Entry entry = owned(user, Right.USE, handle);
        if (entry == null) return false;
        entries.remove(handle); // An authorized attempt consumes the proof even if decryption fails.
        byte[] plaintext = null;
        byte[] digest = null;
        try {
            if (commitment == null || commitment.length != 32) return false;
            Cipher cipher = Cipher.getInstance(TRANSFORM);
            cipher.init(Cipher.DECRYPT_MODE, key(false), new GCMParameterSpec(TAG_BITS, entry.iv));
            bindContext(cipher, user, handle);
            plaintext = cipher.doFinal(entry.ciphertext);
            digest = MessageDigest.getInstance("SHA-256").digest(plaintext);
            return MessageDigest.isEqual(digest, commitment);
        } catch (GeneralSecurityException | IOException | RuntimeException unavailable) {
            return false;
        } finally {
            if (plaintext != null) Arrays.fill(plaintext, (byte) 0);
            if (digest != null) Arrays.fill(digest, (byte) 0);
            entry.clear();
        }
    }

    synchronized boolean release(Grant manager, String handle) {
        Entry entry = owned(manager, Right.MANAGE, handle);
        if (entry == null) return false;
        entries.remove(handle);
        entry.clear();
        return true;
    }

    @Override public synchronized void close() {
        closed = true;
        for (Entry entry : entries.values()) entry.clear();
        entries.clear();
    }
}
