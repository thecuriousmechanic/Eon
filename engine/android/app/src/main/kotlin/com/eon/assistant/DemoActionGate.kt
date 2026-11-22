package com.eon.assistant

internal enum class DemoAction { CLICK_COUNTER, SCROLL_SAMPLES_FORWARD }
internal enum class DemoRoute { NATIVE, LOCAL_HIT }
internal data class DemoScope(val connectionEpoch: Any, val windowToken: Any)
internal data class DemoOffer(val route: DemoRoute, val before: Int, val maxDelta: Int)

/**
 * Main-thread only. The surface supplies exactly two native controls from its private demo tree.
 * A review identifies an action, not an address: the exact child and live scope must still match.
 */
internal class DemoActionGate<N : Any>(
    private val counter: N,
    private val samples: N,
    private val findControl: (DemoAction) -> N?,
    private val offer: (N, DemoAction) -> DemoOffer?,
    private val scope: () -> DemoScope?,
    private val now: () -> Long,
    private val readValue: (N, DemoAction) -> Int?,
    private val dispatch: (N, DemoAction, DemoOffer) -> Boolean,
) {
    companion object { const val REVIEW_MS = 15_000L }

    class Review internal constructor(val action: DemoAction, val offer: DemoOffer, internal val scope: DemoScope,
                                      internal val started: Long)
    private enum class Phase { READY, DISPATCHING, AWAITING_EFFECT, VERIFYING }

    private var current: Review? = null
    private var phase = Phase.READY
    private var generation = 0L
    fun cancel() { current = null; generation++ }

    private fun resolve(action: DemoAction): N? = when (action) {
        DemoAction.CLICK_COUNTER -> findControl(action)?.takeIf { it === counter }
        DemoAction.SCROLL_SAMPLES_FORWARD -> findControl(action)?.takeIf { it === samples }
    }

    fun review(action: DemoAction): Review? {
        cancel()
        val generationAtStart = generation
        val environment = scope() ?: return null
        val node = resolve(action) ?: return null
        val proposal = offer(node, action) ?: return null
        if (proposal.before < 0 || proposal.maxDelta <= 0 || resolve(action) !== node ||
            scope() != environment || generation != generationAtStart) return null
        return Review(action, proposal, environment, now()).also { current = it; phase = Phase.READY }
    }

    private fun valid(r: Review) =
        scope() == r.scope && current === r && now() - r.started in 0 until REVIEW_MS

    /** True means a dispatch was accepted, NOT that the action succeeded. Never try another route. */
    fun confirm(review: Review): Boolean {
        if (current !== review || phase != Phase.READY) return false
        phase = Phase.DISPATCHING
        try {
            if (!valid(review)) return false
            val node = resolve(review.action) ?: return false
            if (offer(node, review.action) != review.offer || resolve(review.action) !== node || !valid(review)) return false
            if (!dispatch(node, review.action, review.offer) || !valid(review)) return false
            phase = Phase.AWAITING_EFFECT
            return true
        } finally {
            if (current === review && phase == Phase.DISPATCHING) cancel()
        }
    }

    /** Read-only, including for a native scroll whose first frame arrived after dispatch. */
    fun verify(review: Review): Boolean {
        if (current !== review || phase != Phase.AWAITING_EFFECT) return false
        phase = Phase.VERIFYING
        try {
            if (!valid(review)) return false
            val node = resolve(review.action) ?: return false
            val after = readValue(node, review.action) ?: return false
            val delta = after.toLong() - review.offer.before
            return resolve(review.action) === node && valid(review) && when (review.action) {
                DemoAction.CLICK_COUNTER -> delta == 1L
                DemoAction.SCROLL_SAMPLES_FORWARD -> delta in 1L..review.offer.maxDelta.toLong()
            }
        } finally {
            if (current === review) cancel()
        }
    }
}
