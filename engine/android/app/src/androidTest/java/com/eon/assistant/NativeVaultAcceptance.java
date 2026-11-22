package com.eon.assistant;

import android.app.Activity;
import android.app.Instrumentation;
import android.os.Bundle;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyInfo;
import android.security.keystore.KeyProperties;

import java.security.KeyStore;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.Set;

import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.SecretKeyFactory;

/** Targets the debug APK's actual libeon_uniffi.so, not a test build of the provider. */
public final class NativeVaultAcceptance extends Instrumentation {
    private static final String PROVIDER = "AndroidKeyStore";
    private static final String ALIAS = "com.eon.foundation.credential-broker.aes-gcm.v1";
    private static final int PURPOSES = KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT;
    private static native boolean checkFixture();

    @Override public void onCreate(Bundle arguments) {
        super.onCreate(arguments);
        start();
    }

    private static void check(boolean ok) {
        if (!ok) throw new AssertionError();
    }

    @Override public void onStart() {
        Bundle result = new Bundle();
        String step = "bootstrap";
        try {
            check(getTargetContext().getApplicationContext() instanceof FoundationVaultApplication);
            // Deliberately do not System.loadLibrary here: the *product* must register the VM.
            KeyStore store = KeyStore.getInstance(PROVIDER);
            store.load(null);
            Set<String> initial = new HashSet<>(Collections.list(store.aliases()));
            check(!initial.contains(ALIAS)); // Never delete/replace an existing user's key.
            try {
                step = "rust-owner-gcm-child-stdin";
                check(checkFixture());
                step = "fixed-nonexportable-key";
                Set<String> expected = new HashSet<>(initial);
                expected.add(ALIAS);
                check(expected.equals(new HashSet<>(Collections.list(store.aliases()))));
                SecretKey key = (SecretKey) store.getKey(ALIAS, null);
                check(key != null && key.getEncoded() == null && key.getFormat() == null);
                KeyInfo info = (KeyInfo) SecretKeyFactory.getInstance("AES", PROVIDER)
                        .getKeySpec(key, KeyInfo.class);
                check(info.getOrigin() == KeyProperties.ORIGIN_GENERATED && info.getKeySize() == 256);
                check(info.getPurposes() == PURPOSES && Arrays.equals(info.getBlockModes(), new String[] {"GCM"})
                        && Arrays.equals(info.getEncryptionPaddings(), new String[] {"NoPadding"}));
                step = "reject-weak-key";
                store.deleteEntry(ALIAS);
                KeyGenerator generator = KeyGenerator.getInstance("AES", PROVIDER);
                generator.init(new KeyGenParameterSpec.Builder(ALIAS, PURPOSES)
                        .setKeySize(128).setBlockModes("GCM").setEncryptionPaddings("NoPadding").build());
                generator.generateKey();
                check(!checkFixture());
            } finally {
                store.deleteEntry(ALIAS);
            }
            check(initial.equals(new HashSet<>(Collections.list(store.aliases()))));
            result.putString("nativeVault", "PASS");
            finish(Activity.RESULT_OK, result);
        } catch (Exception | AssertionError | LinkageError failure) {
            result.putString("nativeVault", "FAIL-" + step);
            finish(Activity.RESULT_CANCELED, result);
        }
    }
}
