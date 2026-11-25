plugins {
    kotlin("jvm")
}

// Use the stock SDK tools directly: no AGP artifact or Android Studio installation is required.
val sdk = providers.environmentVariable("ANDROID_HOME").orElse(providers.environmentVariable("ANDROID_SDK_ROOT"))
val androidJar = sdk.map { file("$it/platforms/android-35/android.jar") }
val buildTools = sdk.map { file("$it/build-tools/37.0.0") }
val jnaAndroid by configurations.creating
val apkClasspath by configurations.creating {
    extendsFrom(configurations.implementation.get())
    exclude(group = "net.java.dev.jna", module = "jna") // APK uses classes.jar and JNI from the Android AAR.
    attributes { attribute(Usage.USAGE_ATTRIBUTE, objects.named(Usage.JAVA_RUNTIME)) }
}

dependencies {
    compileOnly(files(androidJar))
    implementation(project(":core-bindings"))
    jnaAndroid("net.java.dev.jna:jna:${property("jnaVersion")}@aar")
    testImplementation(kotlin("test"))
}

val resDir = file("src/main/res")
val manifest = file("src/main/AndroidManifest.xml")
tasks.test {
    useJUnitPlatform()
    inputs.files(manifest, fileTree(resDir))
}
val compiledRes = layout.buildDirectory.file("intermediates/debug/compiled-res.zip")
val resourcesApk = layout.buildDirectory.file("intermediates/debug/resources.apk")
val generatedR = layout.buildDirectory.dir("generated/debug/r")
val compileDebugResources by tasks.registering(Exec::class) {
    inputs.files(fileTree(resDir))
    inputs.file(buildTools.map { it.resolve("aapt2") })
    outputs.file(compiledRes)
    onlyIf { resDir.isDirectory }
    doFirst {
        val out = compiledRes.get().asFile
        out.parentFile.mkdirs()
        out.delete()
        commandLine(buildTools.get().resolve("aapt2"), "compile", "--dir", resDir, "-o", out)
    }
}
val linkDebugResources by tasks.registering(Exec::class) {
    dependsOn(compileDebugResources)
    inputs.files(manifest, androidJar, fileTree(resDir))
    inputs.file(buildTools.map { it.resolve("aapt2") })
    outputs.file(resourcesApk)
    outputs.dir(generatedR)
    doFirst {
        val out = resourcesApk.get().asFile
        out.parentFile.mkdirs()
        val java = generatedR.get().asFile.apply { mkdirs() }
        commandLine(listOf(buildTools.get().resolve("aapt2"), "link", "--debug-mode", "-I", androidJar.get(),
            "--manifest", manifest, "--java", java, "-o", out) +
            if (resDir.isDirectory) listOf(compiledRes.get().asFile) else emptyList())
    }
}
sourceSets.main { java.srcDir(generatedR) }
tasks.named("compileJava") { dependsOn(linkDebugResources) }
tasks.named("compileKotlin") { dependsOn(linkDebugResources) }

val abis = listOf("arm64-v8a", "armeabi-v7a", "x86_64")
val buildNativeAndroid by tasks.registering(Exec::class) {
    workingDir = rootProject.file("../core-rs")
    val cargoTargetDir = providers.environmentVariable("CARGO_TARGET_DIR").map { rootProject.file(it) }
        .orElse(rootProject.layout.buildDirectory.dir("cargo").map { it.asFile })
    environment("CARGO_TARGET_DIR", cargoTargetDir.get())
    environment("CARGO_PROFILE_DEV_STRIP", "debuginfo")
    // r27 needs an explicit max page size for stock devices with 16 KiB pages.
    for (triple in listOf("AARCH64_LINUX_ANDROID", "X86_64_LINUX_ANDROID", "ARMV7_LINUX_ANDROIDEABI")) {
        environment("CARGO_TARGET_${triple}_RUSTFLAGS", "-Clink-arg=-Wl,-z,max-page-size=16384")
    }
    commandLine(listOf("cargo", "ndk") + abis.flatMap { listOf("-t", it) } + listOf(
        "--platform", "28", "-o", layout.buildDirectory.dir("jni").get().asFile,
        "build", "--locked", "--jobs", "2", "-p", "eon-uniffi", "--lib",
        "--no-default-features", "--features", "profile-mobile-edge",
    ))
}

val assembleDebug by tasks.registering(Exec::class) {
    description = "Build, debug-sign and verify the API 28+ stock Android APK for all three ABIs."
    group = "build"
    dependsOn(buildNativeAndroid, tasks.jar, apkClasspath)
    doFirst {
        commandLine(listOf(
            "python3", file("package_debug.py"), buildTools.get(), androidJar.get(), layout.buildDirectory.get().asFile,
            resourcesApk.get().asFile, jnaAndroid.singleFile, tasks.jar.get().archiveFile.get().asFile,
        ) + apkClasspath.files + listOf("--abis") + abis)
    }
}
