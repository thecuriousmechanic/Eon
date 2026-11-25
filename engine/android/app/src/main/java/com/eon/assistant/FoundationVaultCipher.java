package com.eon.assistant;

import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyInfo;
import android.security.keystore.KeyProperties;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.security.GeneralSecurityException;
import java.security.Key;
import java.security.KeyStore;
import java.util.Arrays;

import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.SecretKeyFactory;
import javax.crypto.spec.GCMParameterSpec;

/** Fixed JNI implementation detail of the Rust Store. No Java entry map or identity authority. */
final class FoundationVaultCipher {
    private static final String PROVIDER = "AndroidKeyStore";
    private static final String ALIAS = "com.eon.foundation.credential-broker.aes-gcm.v1";
    private static final String AES = KeyProperties.KEY_ALGORITHM_AES;
    private static final int PURPOSES = KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT;
    private static final int IV = 12, TAG = 16;
    private static final String TRANSFORM = "AES/GCM/NoPadding";

    private FoundationVaultCipher() {}

    private static synchronized SecretKey key(boolean create) throws GeneralSecurityException, IOException {
        KeyStore store = KeyStore.getInstance(PROVIDER);
        store.load(null);
        if (create && !store.containsAlias(ALIAS)) {
            KeyGenerator generator = KeyGenerator.getInstance(AES, PROVIDER);
            generator.init(new KeyGenParameterSpec.Builder(ALIAS, PURPOSES)
                    .setKeySize(256).setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setRandomizedEncryptionRequired(true).build());
            generator.generateKey();
        }
        Key candidate = store.getKey(ALIAS, null);
        if (!(candidate instanceof SecretKey) || !AES.equals(candidate.getAlgorithm())) {
            throw new GeneralSecurityException();
        }
        SecretKey key = (SecretKey) candidate;
        KeyInfo info = (KeyInfo) SecretKeyFactory.getInstance(AES, PROVIDER).getKeySpec(key, KeyInfo.class);
        // Check the provider's key handle/authorizations, not just the type or existing alias.
        if (key.getFormat() != null || !ALIAS.equals(info.getKeystoreAlias())
                || info.getOrigin() != KeyProperties.ORIGIN_GENERATED || info.getKeySize() != 256
                || info.getPurposes() != PURPOSES
                || !Arrays.equals(info.getBlockModes(), new String[] {KeyProperties.BLOCK_MODE_GCM})
                || !Arrays.equals(info.getEncryptionPaddings(), new String[] {KeyProperties.ENCRYPTION_PADDING_NONE})) {
            throw new GeneralSecurityException();
        }
        return key;
    }

    // Both views are borrowed from Rust for this synchronous call only. Do not retain them
    // or give them to Cipher; only our wipeable Java arrays cross into the JCA provider.
    private static void seal(byte[] aad, ByteBuffer source, ByteBuffer dest)
            throws GeneralSecurityException, IOException {
        Cipher cipher = Cipher.getInstance(TRANSFORM);
        cipher.init(Cipher.ENCRYPT_MODE, key(true)); // Keystore generates the nonce.
        cipher.updateAAD(aad);
        byte[] iv = cipher.getIV();
        byte[] clear = new byte[source.remaining()];
        byte[] ciphertext = null;
        try {
            if (iv == null || iv.length != IV || dest.remaining() != IV + clear.length + TAG) {
                throw new GeneralSecurityException();
            }
            source.get(clear);
            ciphertext = cipher.doFinal(clear);
            if (ciphertext.length != clear.length + TAG) throw new GeneralSecurityException();
            dest.put(iv).put(ciphertext);
        } finally {
            Arrays.fill(clear, (byte) 0);
            if (iv != null) Arrays.fill(iv, (byte) 0);
            if (ciphertext != null) Arrays.fill(ciphertext, (byte) 0);
        }
    }

    private static void unseal(byte[] aad, ByteBuffer source, ByteBuffer dest)
            throws GeneralSecurityException, IOException {
        if (source.remaining() != IV + dest.remaining() + TAG) throw new GeneralSecurityException();
        byte[] iv = new byte[IV];
        byte[] ciphertext = new byte[source.remaining() - IV];
        byte[] clear = null;
        try {
            source.get(iv).get(ciphertext);
            Cipher cipher = Cipher.getInstance(TRANSFORM);
            cipher.init(Cipher.DECRYPT_MODE, key(false), new GCMParameterSpec(TAG * 8, iv));
            cipher.updateAAD(aad);
            clear = cipher.doFinal(ciphertext); // Authenticate before copying even one byte out.
            if (clear.length != dest.remaining()) throw new GeneralSecurityException();
            dest.put(clear);
        } finally {
            Arrays.fill(iv, (byte) 0);
            Arrays.fill(ciphertext, (byte) 0);
            if (clear != null) Arrays.fill(clear, (byte) 0);
        }
    }
}
