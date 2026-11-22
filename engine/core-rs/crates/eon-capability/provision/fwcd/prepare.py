"""Source overlay for the pinned fwcd 1.3.13 distribution. No project build."""
from pathlib import Path
import sys
import zipfile

archive = zipfile.ZipFile(sys.argv[1])
out = Path(sys.argv[2])


def source(name):
    paths = [p for p in archive.namelist()
             if p.endswith("/" + name) and "/src/main/kotlin/" in p]
    assert len(paths) == 1
    return archive.read(paths[0]).decode()


def replace(s, old, new):
    assert s.count(old) == 1, old
    return s.replace(old, new)


def cut(s, start, end, new=""):
    a = s.index(start)
    b = s.index(end, a)
    return s[:a] + new + s[b:]


# No classpath resolver, background discovery, or build-output directory.
s = source("CompilerClassPath.kt")
s = replace(s, "val classPath = mutableSetOf<ClassPathEntry>()",
            'val classPath = setOf("kotlin-stdlib.jar", "java-base.jar").map { ClassPathEntry(Path.of(it).toAbsolutePath(), null) }.toSet()')
s = replace(s, 'Files.createTempDirectory("klsBuildOutput").toFile()', 'File("/eon-no-output")')
s = cut(s, "    private val async =", "    override fun close()", """
    init { compiler.updateConfiguration(config) }
    // The fixed RPC dispatcher does not expose workspace or configuration events.
""")
s = replace(s, "        outputDirectory.delete()", "")
(out / "CompilerClassPath.kt").write_text(s)

# Keep fwcd's JVM analysis; remove scripting registration, global JDK
# discovery and code generation. java-base.jar is data extracted from the
# pinned JDK by the operator, not a path selected by a project.
s = source("Compiler.kt")
s = replace(s, "    val scripts: ScriptDefinitionProvider", "")
s = cut(s, "                add(ComponentRegistrar.", "                addJvmClasspathRoots",
        "                put(JVMConfigurationKeys.NO_JDK, true)\n"
        "                put(JVMConfigurationKeys.USE_PSI_CLASS_FILES_READING, true)\n")
s = cut(s, "                if (scriptsConfig.enabled)", "            },\n            configFiles")
s = cut(s, "        // hacky way", "        val project = environment.project")
s = replace(s, "        scripts = ScriptDefinitionProvider.getInstance(project)!! as CliScriptDefinitionProvider", "")
# KotlinCoreEnvironment.createPackagePartProvider registers each provider in
# a project-lifetime list. The JVM facade asks for the source module's
# scope (only snapshot PSI; no binary roots) and its complement (both jars).
# Never retain either edit-specific scope in the environment.
s = replace(s, "    fun createContainer(sourcePath:", """
    private val binaryParts by lazy {
        environment.createPackagePartProvider(com.intellij.psi.search.GlobalSearchScope.allScope(environment.project))
    }

    fun createContainer(sourcePath:""")
s = replace(s, "        val container = TopDownAnalyzerFacadeForJVM.createContainer(",
            "        val sourceScope = TopDownAnalyzerFacadeForJVM.newModuleSearchScope(environment.project, sourcePath)\n"
            "        val container = TopDownAnalyzerFacadeForJVM.createContainer(\n"
            "            sourceModuleSearchScope = sourceScope,")
s = replace(s, "packagePartProvider = environment::createPackagePartProvider,",
            "packagePartProvider = { scope -> if (scope === sourceScope) org.jetbrains.kotlin.load.kotlin.PackagePartProvider.Empty else binaryParts },")
s = cut(s, "        if (kind == CompilationKind.BUILD_SCRIPT)", "        compileLock.withLock",
        "        require(kind == CompilationKind.DEFAULT)\n")
s = cut(s, "    fun removeGeneratedCode", "    override fun close()",
        '    fun removeGeneratedCode(files: Collection<KtFile>) { require(files.isEmpty()) }\n'
        '    fun generateCode(module: ModuleDescriptor, bindingContext: BindingContext, files: Collection<KtFile>): Unit = error("no codegen")\n')
(out / "Compiler.kt").write_text(s)

# Open buffers only; no DB, writes, disk fallback, or background indexing.
s = source("SourcePath.kt")
s = replace(s, "val index = SymbolIndex(databaseService)", 'val index: SymbolIndex get() = error("no database")')
s = cut(s, "        if (uri !in files)", "        return files[uri]!!")
s = replace(s, "    fun put(uri:", "    fun invalidateSnapshots() = files.values.forEach { it.clean() }\n\n    fun put(uri:")
s = cut(s, "    private fun refreshWorkspaceIndexes", "    fun all(includeHidden:",
        "    private fun refreshWorkspaceIndexes(oldFiles: List<SourceFile>, newFiles: List<SourceFile>) {}\n"
        "    private fun refreshDependencyIndexes(module: ModuleDescriptor) {}\n")
(out / "SourcePath.kt").write_text(s)

s = source("SourceFiles.kt")
s = replace(s, "exclusions.isURIIncluded(uri)", 'uri.scheme == "file" && uri.path.endsWith(".kt")')
(out / "SourceFiles.kt").write_text(s)

s = source("TemporaryDirectory.kt")
s = replace(s, "Files.createTempDirectory(prefix)", 'Path.of("/eon-no-scratch")')
s = cut(s, "Files.createTempFile(dirPath", "\n}", 'error("no scratch")\n    override fun close() {}')
(out / "TemporaryDirectory.kt").write_text(s)

s = source("GoToDefinition.kt")
s = cut(s, "    if (destination != null)", "    return destination",
        '    if (destination?.uri?.contains("!") == true) return null\n')
(out / "GoToDefinition.kt").write_text(s)

s = source("Hovers.kt")
s = replace(s, "file.referenceAtPoint(cursor)", "file.referenceExpressionAtPoint(cursor)")
s = replace(s, "val javaDoc = getDocString(file, cursor)", 'val javaDoc = ""')  # no completion index
(out / "Hovers.kt").write_text(s)
