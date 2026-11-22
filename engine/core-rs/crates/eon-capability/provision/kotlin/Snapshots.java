package eon.kotlin;

import static eon.kotlin.Protocol.*;
import com.intellij.openapi.util.Disposer;
import com.intellij.openapi.util.TextRange;
import com.intellij.psi.*;
import com.intellij.psi.util.PsiTreeUtil;
import java.util.*;
import org.jetbrains.kotlin.analyzer.common.CommonResolverForModuleFactory;
import org.jetbrains.kotlin.cli.common.messages.MessageCollector;
import org.jetbrains.kotlin.cli.FrontendConfigurationKeysKt;
import org.jetbrains.kotlin.cli.jvm.compiler.*;
import org.jetbrains.kotlin.compiler.plugin.CompilerPluginRegistrar.ExtensionStorage;
import org.jetbrains.kotlin.config.*;
import org.jetbrains.kotlin.descriptors.DeclarationDescriptor;
import org.jetbrains.kotlin.descriptors.FunctionDescriptor;
import org.jetbrains.kotlin.diagnostics.rendering.DefaultErrorMessages;
import org.jetbrains.kotlin.js.parser.sourcemaps.*;
import org.jetbrains.kotlin.name.Name;
import org.jetbrains.kotlin.platform.CommonPlatforms;
import org.jetbrains.kotlin.psi.*;
import org.jetbrains.kotlin.renderer.DescriptorRenderer;
import org.jetbrains.kotlin.resolve.*;
import org.jetbrains.kotlin.serialization.deserialization.MetadataPartProvider;

/** One dependency-free module. Never calls a CLI, generator, script host or backend. */
final class Snapshots {
    private final CompilerConfiguration config = new CompilerConfiguration();
    private final com.intellij.openapi.Disposable lifetime = Disposer.newDisposable();
    private final KotlinCoreEnvironment env;
    private final Map<String, KtFile> files = new LinkedHashMap<>();
    private BindingContext bindings;

    Snapshots() {
        config.put(CommonConfigurationKeys.MODULE_NAME, "snapshots");
        config.put(CommonConfigurationKeys.MESSAGE_COLLECTOR_KEY, MessageCollector.Companion.getNONE());
        config.put(JVMConfigurationKeys.NO_JDK, true);
        config.put(JVMConfigurationKeys.DISABLE_STANDARD_SCRIPT_DEFINITION, true);
        FrontendConfigurationKeysKt.setExtensionsStorage(config, new ExtensionStorage());
        env = KotlinCoreEnvironment.createForProduction(lifetime, config, EnvironmentConfigFiles.METADATA_CONFIG_FILES);
    }

    void dispose() { Disposer.dispose(lifetime); }

    void put(String uri, String text, boolean open) {
        if (!uri.endsWith(".kt") || open == files.containsKey(uri) || (open && files.size() >= 16)
                || text.getBytes(java.nio.charset.StandardCharsets.UTF_8).length > 65536)
            throw new IllegalArgumentException("invalid document");
        // createPhysicalFile creates a PSI LightVirtualFile, not an OS file.
        // PSI offsets are UTF-16 in LF-normalized text, like LSP columns.
        var file = new KtPsiFactory(env.getProject(), false).createPhysicalFile(
            uri.substring(uri.lastIndexOf('/') + 1), text.replace("\r\n", "\n").replace('\r', '\n'));
        files.put(uri, file);
        bindings = null;
    }

    void close(String uri) {
        if (files.remove(uri) == null) throw new IllegalArgumentException("document not open");
        bindings = null;
    }

    private void analyze() {
        if (bindings != null) return;
        var result = CommonResolverForModuleFactory.Companion.analyzeFiles(files.values(),
            Name.special("<snapshots>"), true,
            new LanguageVersionSettingsImpl(LanguageVersion.KOTLIN_1_9, ApiVersion.KOTLIN_1_9),
            CommonPlatforms.INSTANCE.getDefaultCommonPlatform(), CompilerEnvironment.INSTANCE,
            Map.of(), null, null, ignored -> MetadataPartProvider.Empty.INSTANCE);
        result.throwIfError();
        bindings = result.getBindingContext();
    }

    private static JsonNode position(KtFile f, int offset) {
        var doc = Objects.requireNonNull(f.getViewProvider().getDocument());
        int line = doc.getLineNumber(offset);
        return obj("line", line, "character", offset - doc.getLineStartOffset(line));
    }

    private static JsonNode range(KtFile f, TextRange r) {
        return obj("start", position(f, r.getStartOffset()), "end", position(f, r.getEndOffset()));
    }

    private static PsiElement identifier(PsiElement e) {
        return e instanceof KtNamedDeclaration d ? d.getNameIdentifier() : e;
    }

    private JsonNode location(PsiElement e) {
        if (e != null) {
            for (var entry : files.entrySet()) {
                if (entry.getValue() == e.getContainingFile()) {
                    return obj("uri", entry.getKey(), "range", range(entry.getValue(), e.getTextRange()));
                }
            }
        }
        return null;
    }

    private static boolean fuzzy(String name, String query) {
        int j = 0;
        for (int i = 0; i < name.length() && j < query.length(); i++) {
            if (Character.toLowerCase(name.charAt(i)) == Character.toLowerCase(query.charAt(j))) j++;
        }
        return j == query.length();
    }

    private static int symbolKind(KtNamedDeclaration d) {
        if (d instanceof KtEnumEntry) return 22;
        if (d instanceof KtClass c) return c.isInterface() ? 11 : c.isEnum() ? 10 : 5;
        if (d instanceof KtObjectDeclaration) return 19;
        if (d instanceof KtNamedFunction) return 12;
        if (d instanceof KtProperty p) return p.isLocal() ? 13 : 7;
        if (d instanceof KtTypeParameter) return 26;
        return 13;
    }

    JsonNode symbols(String query) {
        if (query.isBlank() || query.getBytes(java.nio.charset.StandardCharsets.UTF_8).length > 256)
            throw new IllegalArgumentException("invalid symbol query");
        var matches = new Results();
        for (var entry : files.entrySet()) {
            var file = entry.getValue();
            boolean finished = PsiTreeUtil.processElements(file, e -> {
                if (e instanceof KtNamedDeclaration d && d.getNameIdentifier() != null) {
                    String name = d.getName();
                    if (name != null && name.length() <= 1024 && fuzzy(name, query) &&
                        name.getBytes(java.nio.charset.StandardCharsets.UTF_8).length <= 1024) {
                        var loc = obj("uri", entry.getKey(), "range", range(file, d.getNameIdentifier().getTextRange()));
                        matches.add(obj("name", name, "kind", symbolKind(d), "location", loc));
                    }
                }
                return !matches.full();
            });
            if (!finished) break;
        }
        return matches.array();
    }

    private DeclarationDescriptor resolve(KtFile f, JsonNode pos, boolean references) {
        var doc = Objects.requireNonNull(f.getViewProvider().getDocument());
        int line = integer(pos, "line"), column = integer(pos, "character");
        if (line < 0 || line >= doc.getLineCount() || column < 0 ||
            column >= doc.getLineEndOffset(line) - doc.getLineStartOffset(line)) return null;
        var leaf = f.findElementAt(doc.getLineStartOffset(line) + column);
        for (var e = leaf; e != null; e = e.getParent()) {
            if (references && e instanceof KtArrayAccessExpression)
                throw new RequestError("Implicit operator reference search is unsupported");
            if (e instanceof KtReferenceExpression ref)
                return bindings.get(BindingContext.REFERENCE_TARGET, ref);
            if (e instanceof KtNamedDeclaration d && identifier(d) == leaf)
                return bindings.get(BindingContext.DECLARATION_TO_DESCRIPTOR, d);
        }
        return null;
    }

    private static JsonNode diagnostic(KtFile f, TextRange r, String message) {
        // The host exposes message and range, not LSP's optional source field.
        return obj("range", range(f, r), "message", boundedText("[Kotlin common 1.9, built-ins only] " + message, 4096));
    }

    JsonNode query(String method, String uri, JsonNode params) {
        var f = Objects.requireNonNull(files.get(uri));
        analyze();
        if (method.equals("textDocument/diagnostic")) {
            var items = new Results();
            for (var d : bindings.getDiagnostics().all()) {
                if (d.getPsiFile() == f && d.isValid())
                    for (var r : d.getTextRanges())
                        items.add(diagnostic(f, r, DefaultErrorMessages.render(d)));
            }
            PsiTreeUtil.processElements(f, e -> {
                if (e instanceof PsiErrorElement error)
                    items.add(diagnostic(f, e.getTextRange(), error.getErrorDescription()));
                return true;
            });
            return obj("kind", "full", "items", items.array());
        }
        var resolved = resolve(f, field(params, "position"), method.equals("textDocument/references"));
        if (resolved == null) return JsonNull.INSTANCE;
        var descriptor = resolved.getOriginal();
        if (method.equals("textDocument/hover"))
            return obj("contents", obj("kind", "plaintext", "value",
                boundedText(DescriptorRenderer.COMPACT_WITH_SHORT_TYPES.render(descriptor), 8192)));
        var target = location(identifier(DescriptorToSourceUtils.descriptorToDeclaration(descriptor)));
        if (method.equals("textDocument/definition"))
            return target != null ? target : JsonNull.INSTANCE;
        if (descriptor instanceof FunctionDescriptor func && func.isOperator())
            throw new RequestError("Implicit operator reference search is unsupported");
        var refs = new Results();
        if (target != null) refs.add(target);
        for (var file : files.values()) {
            PsiTreeUtil.processElements(file, e -> {
                if (e instanceof KtSimpleNameExpression ref) {
                    var bound = bindings.get(BindingContext.REFERENCE_TARGET, ref);
                    if (bound != null && bound.getOriginal().equals(descriptor)) refs.add(location(ref));
                }
                return true;
            });
        }
        return refs.array();
    }
}
