package com.eon.assistant

import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith

class ToneLevelsTest {
    private fun samples(n: Int, sample: (Int) -> Int) = ByteBuffer.allocate(2 * n).apply {
        order(ByteOrder.nativeOrder())
        repeat(n) { putShort(sample(it).toShort()) }
        flip()
    }

    @Test
    fun stereoRmsNotPeakOrSignedMeanAndPartialLastBinUsesActualFrames() {
        val bins = mutableListOf<Int>()
        val scan = ToneLevels(8000, 2, bins::add)
        // 800 stereo frames with one silent channel, including signed PCM minimum.
        scan.append(samples(790) { if (it % 2 == 0) -32768 else 0 })
        assertEquals(emptyList(), bins)
        scan.append(samples(810) { if (it % 2 == 0) 32767 else 0 })
        scan.append(samples(2) { -16384 })
        scan.finish()
        assertEquals(listOf(71, 50), bins)
    }

    @Test
    fun hardWindowLimitAndSilenceNeverInventAFloorOrTrailingLevels() {
        val bins = mutableListOf<Int>()
        val scan = ToneLevels(8000, 1, bins::add)
        val pcm = samples(800 * 30) { 0 }
        scan.append(pcm)
        scan.finish()
        assertEquals(List(12) { 0 }, bins)
        assertEquals(2 * 800 * 12, pcm.position())
    }

    @Test
    fun microphoneUsesTwelveHalfSecondWindowsAndDropsFurtherInput() {
        val bins = mutableListOf<Int>()
        val scan = ToneLevels(16_000, 1, 2, bins::add)
        scan.append(samples(7999) { 0 })
        assertEquals(emptyList(), bins)
        scan.append(samples(1) { 0 })
        scan.append(samples(8000 * 20) { if (it / 8000 % 2 == 0) -16384 else 8192 })
        assertEquals(listOf(0, 50, 25, 50, 25, 50, 25, 50, 25, 50, 25, 50), bins)
    }

    @Test
    fun rejectsEmptyPartialFramesAndOutOfBoundsFormats() {
        for ((rate, channels) in listOf(7999 to 1, 96001 to 1, 8000 to 0, 8000 to 3)) {
            assertFailsWith<IllegalArgumentException> { ToneLevels(rate, channels) {} }
        }
        val scan = ToneLevels(96000, 2) { error("No complete frames") }
        assertFailsWith<IllegalArgumentException> { scan.append(ByteBuffer.allocate(3)) }
        assertFailsWith<IllegalStateException> { scan.finish() }
    }
}
