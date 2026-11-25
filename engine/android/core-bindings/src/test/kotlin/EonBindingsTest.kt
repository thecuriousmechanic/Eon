import uniffi.eon_uniffi.BridgeException
import uniffi.eon_uniffi.Completeness
import uniffi.eon_uniffi.DagNode
import uniffi.eon_uniffi.MobileWorkflowException
import uniffi.eon_uniffi.endpointSilenceMs
import uniffi.eon_uniffi.executeTaskSync
import uniffi.eon_uniffi.generateUlid
import uniffi.eon_uniffi.getMonotonicTimeNanos
import uniffi.eon_uniffi.injectAudioPcmForTesting
import uniffi.eon_uniffi.runProfileWorkflow
import uniffi.eon_uniffi.selectModelProfile
import uniffi.eon_uniffi.validateTaskDag
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

class EonBindingsTest {
    @Test
    fun profileWorkflowReturnsOnlyThisRunsCompletionOrThrows() {
        val first = runProfileWorkflow(4.0)
        assertEquals("p0_mobile_ultralight", first.profile)
        assertEquals(1u, first.completedTasks)
        assertFailsWith<MobileWorkflowException.InvalidInput> { runProfileWorkflow(-1.0) }
        assertFailsWith<MobileWorkflowException.InvalidInput> { runProfileWorkflow(Double.NaN) }
        val next = runProfileWorkflow(8.0)
        assertEquals("p1_mobile_flagship", next.profile)
        assertEquals(1u, next.completedTasks)
    }

    @Test
    fun existingBridgeErrorsRemainExhaustive() {
        fun classify(error: BridgeException) = when (error) {
            is BridgeException.InvalidInput -> "invalid"
            is BridgeException.UnsupportedTask -> "unsupported"
        }
        assertEquals("invalid", classify(assertFailsWith<BridgeException> { selectModelProfile(Double.NaN) }))
        val unsupported = assertFailsWith<BridgeException> {
            executeTaskSync(generateUlid("task"), "unknown", "unknown", "{}")
        }
        assertEquals("unsupported", classify(unsupported))
    }

    @Test
    fun deviceMemorySelectsTheMatchingModelProfile() {
        assertEquals("p3_workstation_heavy", selectModelProfile(16.0))
        assertEquals("p0_mobile_ultralight", selectModelProfile(4.0))
    }

    @Test
    fun endpointTiersMatchTheSpecThresholds() {
        assertEquals(300u, endpointSilenceMs(Completeness.COMPLETE))
        assertEquals(800u, endpointSilenceMs(Completeness.AMBIGUOUS))
        assertEquals(1150u, endpointSilenceMs(Completeness.INCOMPLETE))
    }

    @Test
    fun invalidMemoryIsRaisedAsATypedException() {
        val error = assertFailsWith<BridgeException.InvalidInput> { selectModelProfile(Double.NaN) }
        assertTrue(error.message.contains("finite"), error.message)
    }

    @Test
    fun idsAndTimeUseKernelPrimitives() {
        val first = generateUlid("task")
        val next = generateUlid("task")
        assertTrue(first.matches(Regex("task_[0-7][0-9A-HJKMNP-TV-Z]{25}")))
        assertTrue(next > first)
        assertFailsWith<BridgeException.InvalidInput> { generateUlid("unknown") }
        val before = getMonotonicTimeNanos()
        assertTrue(getMonotonicTimeNanos() >= before)
    }

    @Test
    fun callerSuppliedFrameGoesThroughRustLimiter() {
        val result = injectAudioPcmForTesting(List(512) { Short.MIN_VALUE })
        assertEquals(512u, result.sampleCount)
        assertEquals(0.95f, result.appliedGain, 0.00001f)
        assertEquals(0.95f, result.outputPeak, 0.00001f)
        assertFailsWith<BridgeException.InvalidInput> { injectAudioPcmForTesting(emptyList()) }
    }

    @Test
    fun dagValidationReturnsDependencyOrderNotSubmissionOrder() {
        val child = generateUlid("task")
        val root = generateUlid("task")
        val node = DagNode(child, listOf(root))
        assertEquals(listOf(root, child), validateTaskDag(listOf(node, DagNode(root, emptyList()))))
        assertFailsWith<BridgeException.InvalidInput> { validateTaskDag(listOf(node)) }
        assertFailsWith<BridgeException.InvalidInput> { validateTaskDag(listOf(node, DagNode(root, listOf(child)))) }
    }

    @Test
    fun onlyReadOnlyDiagnosticTasksExecute() {
        val id = generateUlid("task")
        val operation = "select_model_profile"
        val params = """{"memory_gb":8}"""
        val result = executeTaskSync(id, "diagnostics", operation, params)
        assertEquals(id, result.taskId)
        assertEquals("completed", result.status)
        assertEquals("""{"profile":"p1_mobile_flagship"}""", result.outcomeJson)
        assertEquals(null, result.errorCode)
        assertFailsWith<BridgeException.InvalidInput> { executeTaskSync(id, "diagnostics", operation, "[]") }
        assertFailsWith<BridgeException.UnsupportedTask> { executeTaskSync(id, "shell", operation, params) }
        val timing = executeTaskSync(id, "diagnostics", "endpoint_silence_ms", """{"completeness":"complete"}""")
        assertEquals("""{"silence_ms":300}""", timing.outcomeJson)
    }
}
