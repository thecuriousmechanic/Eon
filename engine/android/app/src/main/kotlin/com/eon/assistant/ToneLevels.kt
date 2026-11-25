package com.eon.assistant

import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlin.math.roundToInt
import kotlin.math.sqrt

/** Fixed-window integer RMS of interleaved PCM16. No samples survive a call. */
internal class ToneLevels(rate: Int, private val channels: Int, binsPerSecond: Int, private val report: (Int) -> Unit) {
    constructor(rate: Int, channels: Int, report: (Int) -> Unit) : this(rate, channels, 10, report)
    companion object {
        const val LIMIT = 12
    }

    init {
        require(rate in 8_000..96_000 && channels in 1..2 && binsPerSecond in 1..10)
    }
    private val framesPerBin = rate / binsPerSecond
    private var squares = 0L
    private var frames = 0
    var count = 0
        private set

    fun append(pcm: ByteBuffer) {
        pcm.order(ByteOrder.nativeOrder())
        require(pcm.remaining() % (2 * channels) == 0)
        while (pcm.hasRemaining() && count < LIMIT) {
            repeat(channels) {
                val value = pcm.short.toLong()
                squares += value * value
            }
            if (++frames == framesPerBin) publish()
        }
    }

    fun finish() {
        if (frames > 0) publish()
        check(count > 0)
    }

    private fun publish() {
        val level = (100 * sqrt(squares.toDouble() / (frames * channels)) / 32768).roundToInt()
        squares = 0L
        frames = 0
        count++
        report(level)
    }
}
