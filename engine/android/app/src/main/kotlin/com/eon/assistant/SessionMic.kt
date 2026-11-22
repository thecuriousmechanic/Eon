package com.eon.assistant

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioRecord
import android.media.MediaRecorder
import android.os.SystemClock
import java.nio.ByteBuffer

/** A role or pre-existing OS grant alone is not evidence that our runtime request was approved. */
internal object MicPermission {
    const val NAME = Manifest.permission.RECORD_AUDIO
    var reviewed = false // Main thread only. No consent survives process death or observed revocation.
    fun granted(context: Context) = context.checkSelfPermission(NAME) == PackageManager.PERMISSION_GRANTED
    fun allowed(context: Context): Boolean {
        if (!granted(context)) reviewed = false
        return reviewed
    }
}

/** One take. Opened only by the HUD tap, synchronously closed by the visible session's owner. */
internal class SessionMic private constructor(private var recorder: AudioRecord?) : AutoCloseable {
    companion object {
        private const val RATE = 16_000
        private const val CHANNEL = AudioFormat.CHANNEL_IN_MONO
        private const val PCM = AudioFormat.ENCODING_PCM_16BIT
        fun open(): SessionMic {
            val min = AudioRecord.getMinBufferSize(RATE, CHANNEL, PCM)
            check(min in 1..131_072)
            val record = AudioRecord(MediaRecorder.AudioSource.MIC, RATE, CHANNEL, PCM, maxOf(min, RATE / 2))
            try {
                check(record.state == AudioRecord.STATE_INITIALIZED)
                record.startRecording()
                check(record.recordingState == AudioRecord.RECORDSTATE_RECORDING)
                return SessionMic(record)
            } catch (e: RuntimeException) {
                record.release()
                throw e
            }
        }
    }

    private val deadline = SystemClock.elapsedRealtime() + 7_000

    @Synchronized override fun close() {
        val r = recorder ?: return
        recorder = null
        // AudioRecord.release() stops the input and releases native resources.
        r.release()
    }

    @Synchronized private fun read(buffer: ByteBuffer): Int {
        val r = checkNotNull(recorder)
        check(SystemClock.elapsedRealtime() < deadline && r.activeRecordingConfiguration?.isClientSilenced != true)
        return r.read(buffer, buffer.capacity(), AudioRecord.READ_NON_BLOCKING)
    }

    fun measure(context: Context, report: (Int) -> Unit): Boolean {
        // No PCM crosses this worker, reaches the UI, or is sent to another API.
        val buffer = ByteBuffer.allocateDirect(RATE / 5) // 100 ms of mono PCM16.
        val bins = ToneLevels(RATE, 1, 2, report) // 12 half-second windows
        return try {
            val audio = context.getSystemService(AudioManager::class.java)
            while (bins.count < ToneLevels.LIMIT) {
                check(MicPermission.granted(context) && !audio.isMicrophoneMute)
                buffer.clear()
                val n = read(buffer)
                check(n >= 0)
                buffer.limit(n)
                bins.append(buffer)
                buffer.clear()
                while (buffer.remaining() >= 4) buffer.putInt(0)
                if (n == 0) SystemClock.sleep(20)
            }
            true
        } catch (_: RuntimeException) {
            false
        } finally {
            close()
            buffer.clear()
            while (buffer.remaining() >= 4) buffer.putInt(0)
        }
    }
}
