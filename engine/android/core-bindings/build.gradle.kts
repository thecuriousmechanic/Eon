plugins {
    kotlin("jvm")
}

dependencies {
    api("net.java.dev.jna:jna:${property("jnaVersion")}")
    testImplementation(kotlin("test"))
}

// The Rust engine is built and its Kotlin bindings are generated from the same cdylib that the tests load, so the
// bindings and the native library always come from one build.
val engineDir = rootProject.projectDir.resolve("../core-rs")
val cargoTargetDir = providers.environmentVariable("CARGO_TARGET_DIR").map { rootProject.file(it) }
    .orElse(rootProject.layout.buildDirectory.dir("cargo").map { it.asFile })
val nativeLibDir = cargoTargetDir.map { it.resolve("debug") }
val nativeLibrary = nativeLibDir.map { it.resolve(System.mapLibraryName("eon_uniffi")) }
val generatedKotlin = layout.buildDirectory.dir("generated/uniffi/kotlin")

val buildNativeLibrary by tasks.registering(Exec::class) {
    workingDir = engineDir
    environment("CARGO_TARGET_DIR", cargoTargetDir.get())
    commandLine("cargo", "build", "--locked", "--jobs", "2", "-p", "eon-uniffi", "--lib",
        "--features", "bindgen-cli", "--bin", "uniffi-bindgen")
}

val generateBindings by tasks.registering(Exec::class) {
    dependsOn(buildNativeLibrary)
    workingDir = engineDir
    val outDir = generatedKotlin.get().asFile.absolutePath
    environment("CARGO_TARGET_DIR", cargoTargetDir.get())
    commandLine(nativeLibDir.get().resolve("uniffi-bindgen"),
        "generate", "--library", "--metadata-no-deps", nativeLibrary.get(),
        "--language", "kotlin", "--out-dir", outDir, "--no-format",
    )
}

sourceSets {
    main {
        kotlin.srcDir(generatedKotlin)
    }
}

tasks.named("compileKotlin") {
    dependsOn(generateBindings)
}

tasks.test {
    dependsOn(buildNativeLibrary)
    // Rust behavior can change without altering a single generated Kotlin signature.
    inputs.file(nativeLibrary)
    useJUnitPlatform()
    systemProperty("jna.library.path", nativeLibDir.get())
}

tasks.register<Exec>("verifyTargetBindings") {
    group = "verification"
    description = "Compare bindings generated from each packaged Android ELF against the host JVM-tested API."
    dependsOn(tasks.test, ":app:assembleDebug")
    workingDir = engineDir
    environment("CARGO_TARGET_DIR", cargoTargetDir.get())
    commandLine("python3", file("check_abi_bindings.py"), nativeLibDir.get().resolve("uniffi-bindgen"),
        nativeLibrary.get(),
        project(":app").layout.buildDirectory.file("outputs/apk/debug/app-debug.apk").get().asFile,
        layout.buildDirectory.dir("verify-abi-bindings").get().asFile)
}
