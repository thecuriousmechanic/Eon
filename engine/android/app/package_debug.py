"""Build a debug-only APK using the local Android SDK and Gradle's resolved inputs."""

import argparse
from pathlib import Path
import shutil
import subprocess
from zipfile import ZIP_STORED, ZipFile

from verify_apk import verify


def run(*args):
    subprocess.run(args, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for arg in ("tools", "platform", "build", "resources_apk", "jna_aar"):
        parser.add_argument(arg, type=Path)
    parser.add_argument("jars", nargs="+", type=Path)
    parser.add_argument("--abis", nargs="+", required=True)
    args = parser.parse_args()
    tools = args.tools
    work = args.build / "intermediates/debug"
    work.mkdir(parents=True, exist_ok=True)
    apk = args.build / "outputs/apk/debug/app-debug.apk"
    apk.parent.mkdir(parents=True, exist_ok=True)

    raw = work / "raw.apk"
    dex = work / "dex.zip"
    aligned = work / "aligned.apk"
    shutil.copyfile(args.resources_apk, raw)
    with ZipFile(args.jna_aar) as aar:
        jna_jar = work / "jna-android.jar"
        jna_jar.write_bytes(aar.read("classes.jar"))
        run(tools / "d8", "--min-api", "28", "--lib", args.platform, "--output", dex, jna_jar, *args.jars)
        with ZipFile(raw, "a", compression=ZIP_STORED) as out, ZipFile(dex) as dex_files:
            for name in dex_files.namelist():
                if name.endswith(".dex"):
                    out.writestr(name, dex_files.read(name))
            for abi in args.abis:
                # Never package a desktop dispatch library from the host JNA jar.
                out.writestr(f"lib/{abi}/libjnidispatch.so", aar.read(f"jni/{abi}/libjnidispatch.so"))
                engine = args.build / "jni" / abi / "libeon_uniffi.so"
                out.write(engine, f"lib/{abi}/{engine.name}")

    run(tools / "zipalign", "-f", "-P", "16", "4", raw, aligned)
    key = args.build / "debug.keystore"
    if not key.exists():
        run("keytool", "-genkeypair", "-noprompt", "-keystore", key, "-storepass", "android",
            "-keypass", "android", "-alias", "androiddebugkey", "-keyalg", "RSA", "-keysize", "2048",
            "-validity", "10000", "-dname", "CN=Android Debug,O=Android,C=US")
    signed = work / "signed.apk"
    run(tools / "apksigner", "sign", "--ks", key, "--ks-pass", "pass:android",
        "--v4-signing-enabled", "false", "--out", signed, aligned)
    verify(signed, tools, work)
    signed.replace(apk)
    print(f"Verified {apk}")


if __name__ == "__main__":
    main()
