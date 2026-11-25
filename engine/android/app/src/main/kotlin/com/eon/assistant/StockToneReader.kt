package com.eon.assistant

import android.content.ContentUris
import android.content.Context
import android.media.AudioFormat
import android.media.MediaCodec
import android.media.MediaExtractor
import android.media.MediaFormat
import android.os.Bundle
import android.os.CancellationSignal
import android.os.SystemClock
import android.provider.MediaStore.Audio.Media
import java.util.concurrent.atomic.AtomicBoolean

/** Only the first OS-installed notification tone. No caller-supplied URI, user media or playback. */
internal object StockToneReader {
    fun interface Listener { fun onLevel(level: Int) }
    private const val MAX_BYTES = 2L * 1024 * 1024
    private val busy = AtomicBoolean()

    fun read(context: Context, stop: CancellationSignal, listener: Listener) {
        check(busy.compareAndSet(false, true))
        try {
            val deadline = SystemClock.elapsedRealtime() + 5_000
            fun checkActive() {
                stop.throwIfCanceled()
                check(SystemClock.elapsedRealtime() < deadline)
            }
            checkActive()
            val base = Media.INTERNAL_CONTENT_URI
            val query = Bundle().apply {
                putString(android.content.ContentResolver.QUERY_ARG_SQL_SELECTION, "${Media.IS_NOTIFICATION}=1")
                putStringArray(android.content.ContentResolver.QUERY_ARG_SORT_COLUMNS, arrayOf(Media._ID))
                putInt(android.content.ContentResolver.QUERY_ARG_LIMIT, 1)
            }
            val id = context.contentResolver.query(base, arrayOf(Media._ID), query, stop)?.use {
                check(it.moveToFirst())
                it.getLong(0)
            } ?: error("No stock notification tone")
            checkActive()
            val uri = ContentUris.withAppendedId(base, id)
            val asset = checkNotNull(context.contentResolver.openAssetFileDescriptor(uri, "r", stop))
            asset.use {
                require(it.length in 1..MAX_BYTES)
                val extractor = MediaExtractor()
                try {
                    extractor.setDataSource(it.fileDescriptor, it.startOffset, it.length)
                    check(extractor.trackCount == 1)
                    val format = extractor.getTrackFormat(0)
                    val mime = checkNotNull(format.getString(MediaFormat.KEY_MIME))
                    require(mime in setOf("audio/vorbis", "audio/opus", "audio/mpeg", "audio/mp4a-latm", "audio/flac", "audio/raw"))
                    require(format.getInteger(MediaFormat.KEY_SAMPLE_RATE) in 8_000..96_000)
                    require(format.getInteger(MediaFormat.KEY_CHANNEL_COUNT) in 1..2)
                    extractor.selectTrack(0)
                    checkActive()
                    val decoder = MediaCodec.createDecoderByType(mime)
                    try {
                        decoder.configure(format, null, null, 0)
                        decoder.start()
                        decode(extractor, decoder, ::checkActive) {
                            checkActive()
                            listener.onLevel(it)
                        }
                    } finally {
                        decoder.release()
                    }
                } finally {
                    extractor.release()
                }
            }
        } finally {
            busy.set(false)
        }
    }

    private fun decode(extractor: MediaExtractor, codec: MediaCodec, checkActive: () -> Unit, report: (Int) -> Unit) {
        val info = MediaCodec.BufferInfo()
        var endOfInput = false
        var inputBytes = 0L
        var levels: ToneLevels? = null
        while (true) {
            checkActive()
            val input = if (endOfInput) -1 else codec.dequeueInputBuffer(5_000)
            if (input >= 0) {
                val buffer = checkNotNull(codec.getInputBuffer(input))
                val length = extractor.readSampleData(buffer, 0)
                endOfInput = length < 0
                if (endOfInput) {
                    codec.queueInputBuffer(input, 0, 0, 0, MediaCodec.BUFFER_FLAG_END_OF_STREAM)
                } else {
                    inputBytes += length
                    require(inputBytes <= MAX_BYTES)
                    codec.queueInputBuffer(input, 0, length, extractor.sampleTime, 0)
                    extractor.advance()
                }
            }
            val output = codec.dequeueOutputBuffer(info, 5_000)
            if (output < 0) {
                // Refuse to mix windows across a mid-stream format change.
                check(output != MediaCodec.INFO_OUTPUT_FORMAT_CHANGED || levels == null)
                continue
            }
            try {
                if (info.size > 0 && info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) {
                    val format = codec.getOutputFormat(output)
                    require(!format.containsKey(MediaFormat.KEY_PCM_ENCODING) ||
                        format.getInteger(MediaFormat.KEY_PCM_ENCODING) == AudioFormat.ENCODING_PCM_16BIT)
                    val bins = levels ?: ToneLevels(format.getInteger(MediaFormat.KEY_SAMPLE_RATE),
                        format.getInteger(MediaFormat.KEY_CHANNEL_COUNT), report).also { levels = it }
                    // getOutputBuffer returns the position and limit of the valid PCM.
                    val pcm = checkNotNull(codec.getOutputBuffer(output))
                    require(pcm.remaining() == info.size)
                    bins.append(pcm)
                }
            } finally {
                codec.releaseOutputBuffer(output, false)
            }
            if (levels?.count == ToneLevels.LIMIT || info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) {
                checkActive()
                checkNotNull(levels).finish()
                checkActive() // The last partial-bin callback can itself revoke the request.
                return
            }
        }
    }
}
