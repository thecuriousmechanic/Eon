package eon.kotlin

import eon.kotlin.Protocol.*
import org.eclipse.lsp4j.*
import org.javacs.kt.*
import org.javacs.kt.diagnostic.convertDiagnostic
import org.jetbrains.kotlin.js.parser.sourcemaps.*
import org.jetbrains.kotlin.psi.psiUtil.collectDescendantsOfType
import org.jetbrains.kotlin.psi.KtNamedDeclaration
import com.intellij.psi.PsiErrorElement
import org.javacs.kt.position.*
import org.javacs.kt.util.findParent
import java.io.*
import java.net.URI
import java.nio.file.Path

/** Fixed JNI entry point; upstream's general RPC endpoint is not exposed. */
object Worker {
    private data class Open(val version: Int, val wireUri: String)
    private val documents = mutableMapOf<URI, Open>()
    private var reports: Map<URI, List<Diagnostic>>? = null
    private lateinit var server: KotlinLanguageServer
    // HotSpot's native filename encoding need not be UTF-8. fwcd needs only
    // distinct .kt PSI paths, so never convert a caller's filename to a Path.
    private fun uri(raw: String, opening: Boolean): URI {
        val u = URI.create(raw)
        require(raw.length <= 4096 && u.scheme == "file" && u.authority == null &&
            u.query == null && u.fragment == null && u.path.matches(Regex("/eon-lsp-[0-9a-f]{16}/.+\\.kt")) &&
            u.path.split('/').drop(1).all { it.isNotEmpty() && it != "." && it != ".." })
        val existing = documents.entries.find { it.value.wireUri == raw }
        if (existing != null) return existing.key
        require(opening && documents.size < 16)
        return (0..15).map { Path.of("/eon-snapshot/document$it.kt").toUri() }.first { it !in documents }
    }

    // Invalidate the entire module on every mutation (including close).
    // All notifications and requests run in wire order; no asynchronous lint.
    private fun sync(op: String, u: URI, doc: JsonNode, p: JsonNode) {
        val sp = server.sourcePath
        if (op == "didClose") {
            require(documents.remove(u) != null)
            sp.delete(u)
        } else {
            val v = integer(doc, "version")
            val old = documents[u]
            val text = if (op == "didOpen") {
                require(old == null && documents.size < 16)
                string(doc, "text")
            } else {
                require(old != null && v > old.version)
                val change = (field(p, "contentChanges") as JsonArray).elements.single()
                require(field(change, "range") == null)
                string(change, "text")
            }
            require(v > 0 && text.toByteArray().size <= 65536)
            documents[u] = Open(v, string(doc, "uri"))
            sp.put(u, text.replace("\r\n", "\n").replace('\r','\n'), null)
        }
        sp.invalidateSnapshots()
        reports = null
    }

    private fun compile() {
        if (reports != null) return
        val sp = server.sourcePath
        val context = sp.compileFiles(documents.keys)
        val found = mutableMapOf<URI, MutableList<Diagnostic>>()
        fun add(u: URI, d: Diagnostic) {
            val items = found.getOrPut(u) { mutableListOf() }
            if (items.size < 257) items.add(d) // sentinel: don't return a truncated report
        }
        for (d in context.diagnostics) for ((u, item) in convertDiagnostic(d)) add(u, item)
        for (u in documents.keys) {
            val parsed = sp.parsedFile(u)
            for (e in parsed.collectDescendantsOfType<PsiErrorElement>())
                add(u, Diagnostic(range(parsed.text, e.textRange), e.errorDescription))
        }
        reports = found
    }

    private fun pos(p: Position) = obj("line", p.line, "character", p.character)
    private fun span(r: Range) = obj("start", pos(r.start), "end", pos(r.end))
    private fun locations(items: List<Location>): JsonNode {
        val out = Results()
        for (loc in items.distinct()) {
            val u = URI.create(loc.uri)
            val open = documents[u] ?: throw RequestError("Location outside open snapshots")
            out.add(obj("uri", open.wireUri, "range", span(loc.range)))
        }
        return out.array()
    }

    private fun query(op: String, u: URI, p: JsonNode): JsonNode {
        val open = documents[u] ?: throw RequestError("Document is not open")
        compile()
        if (op == "diagnostic") {
            if (integer(p, "x-eon-version") != open.version) throw RequestError("Snapshot version mismatch")
            val out = Results()
            for (d in reports!![u].orEmpty())
                out.add(obj("range", span(d.range), "message", boundedText(d.message, 4096)))
            return obj("kind", "full", "x-eon-version", open.version, "items", out.array())
        }
        val point = field(p, "position")
        val at = Position(integer(point, "line"), integer(point, "character"))
        val line = server.sourcePath.content(u).lineSequence().elementAtOrNull(at.line)
        if (line == null || at.character !in 0..line.length) throw RequestError("Position outside snapshot")
        val ident = TextDocumentIdentifier(u.toASCIIString())
        val service = server.textDocumentService
        return when (op) {
            "definition" -> locations(service.definition(DefinitionParams(ident, at)).get().left)
            "hover" -> service.hover(HoverParams(ident, at)).get()?.let {
                obj("contents", boundedText(it.contents.right.value, 8192))
            } ?: JsonNull
            "references" -> {
                // Upstream searches from declaration sites. Resolve calls first.
                val def = service.definition(DefinitionParams(ident, at)).get().left.singleOrNull() ?: run {
                    val f = server.sourcePath.parsedFile(u)
                    val cursor = offset(f.text, at.line, at.character)
                    val name = f.findElementAt(cursor)?.findParent<KtNamedDeclaration>()?.nameIdentifier
                    if (name == null || !name.textRange.containsOffset(cursor)) return locations(emptyList())
                    location(name) ?: return locations(emptyList())
                }
                if (URI.create(def.uri) !in documents) throw RequestError("External declaration")
                val params = ReferenceParams(TextDocumentIdentifier(def.uri), def.range.start, ReferenceContext(true))
                val uses = service.references(params).get().orEmpty()
                locations(uses + def)
            }
            else -> throw RequestError("Unsupported")
        }
    }

    @JvmStatic fun run() {
        LOG.level = LogLevel.NONE
        LOG.connectErrorBackend {}
        LOG.connectOutputBackend {}
        server = KotlinLanguageServer(Configuration(indexing = IndexingConfiguration(false)))
        val input = BufferedInputStream(object: FileInputStream(FileDescriptor.`in`) {
            override fun available() = 0
        })
        try {
            while (true) {
                val msg = read(input) ?: break
                val id = field(msg, "id")
                val p = field(msg, "params")
                val method = string(msg, "method")
                try {
                    when (method) {
                        "initialize" -> reply(id, "result", obj("serverInfo", obj("name", "fwcd (EON snapshot)", "version", "1.3.13"),
                            "capabilities", obj("textDocumentSync", 1, "definitionProvider", true,
                                "referencesProvider", true, "hoverProvider", true, "diagnosticProvider",
                                obj("interFileDependencies", true, "workspaceDiagnostics", false))))
                        "initialized" -> {}
                        "shutdown" -> reply(id, "result", JsonNull)
                        "exit" -> return
                        "textDocument/didOpen", "textDocument/didChange", "textDocument/didClose",
                        "textDocument/definition", "textDocument/hover", "textDocument/references", "textDocument/diagnostic" -> {
                            val op = method.substringAfter('/')
                            val doc = field(p, "textDocument")
                            val u = uri(string(doc, "uri"), op == "didOpen")
                            if (op.startsWith("did")) sync(op, u, doc, p)
                            else reply(id, "result", query(op, u, p))
                        }
                        else -> if (id != null) reply(id, "error", obj("code", -32601, "message", "Unsupported"))
                    }
                } catch (e: RequestError) {
                    if (id == null) throw e
                    reply(id, "error", obj("code", -32803, "message", e.message))
                }
            }
        } finally { server.close() }
    }
}
