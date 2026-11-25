package com.eon.assistant

import java.security.MessageDigest
import java.security.SecureRandom
import java.util.concurrent.Executor

/** Provisioned only by the foreground confirmation. The shortcut gets no management grant. */
internal class InspectionCredential private constructor(
    private val broker: AndroidCredentialBroker,
    private val use: AndroidCredentialBroker.Grant,
    private val handle: String,
    private val commitment: ByteArray,
) : AutoCloseable {
    companion object {
        fun issue(): InspectionCredential? {
            val authority = AndroidCredentialBroker.Authority()
            val broker = AndroidCredentialBroker(authority)
            val owner = authority.newOwner()
            val nonce = ByteArray(32)
            try {
                SecureRandom().nextBytes(nonce)
                val commitment = MessageDigest.getInstance("SHA-256").digest(nonce)
                val handle = broker.put(owner.manage, nonce) ?: return null
                return InspectionCredential(broker, owner.use, handle, commitment)
            } finally {
                nonce.fill(0)
            }
        }
    }

    fun redeem(): Boolean = broker.consumeInspectionProof(use, handle, commitment)

    override fun close() {
        commitment.fill(0)
        broker.close()
    }
}

/**
 * The worker issues AND verifies the proof during foreground confirmation. Only an unexpired,
 * still-current success arms the main-thread session. The shortcut never waits for a provider
 * round-trip: it consumes that session and reads the selected focus within the same callback.
 */
internal class VerifiedInspectionConsent(
    private val elapsedMillis: () -> Long,
    private val worker: Executor,
    private val main: Executor,
    private val verifyProof: () -> Boolean,
) {
    private val session = OneShotInspection(elapsedMillis)
    private var epoch = Any()
    private var busy = false
    private var provisioning = false

    fun revoke() {
        epoch = Any()
        provisioning = false
        session.revoke()
    }

    fun cancelPending() { if (provisioning) revoke() }

    fun confirm(eligible: () -> Boolean, ready: (Boolean) -> Unit) {
        revoke()
        if (busy || !eligible()) {
            ready(false)
            return
        }
        val request = epoch
        val confirmedAt = elapsedMillis()
        busy = true
        provisioning = true
        worker.execute {
            val verified = try { verifyProof() } catch (_: Exception) { false }
            main.execute {
                busy = false
                if (epoch === request) {
                    provisioning = false
                    ready(session.allowNextShortcut(verified && eligible(), confirmedAt))
                }
            }
        }
    }

    fun inspect(eligible: () -> Boolean, readFocused: () -> ControlKind): Boolean =
        session.inspectOnShortcut(eligible, readFocused)

    fun takeResult(eligible: Boolean): ControlKind? = session.takeResult(eligible)
}
