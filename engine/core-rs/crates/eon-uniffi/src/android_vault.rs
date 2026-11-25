use std::ffi::c_void;

use eon_foundation::credentials::register_android_vm;
use jni::JavaVM;
use jni::sys::{JNI_ERR, JNI_VERSION_1_6, jint};

// Invoked by ART via System.loadLibrary (not JNA's dlopen). This is not a UniFFI API.
#[unsafe(no_mangle)]
extern "system" fn JNI_OnLoad(raw: *mut jni::sys::JavaVM, _: *mut c_void) -> jint {
    // SAFETY: ART supplies its process-lifetime VM; from_raw also rejects null.
    let Ok(vm) = (unsafe { JavaVM::from_raw(raw) }) else {
        return JNI_ERR;
    };
    // SAFETY: ART invokes this entry point in the packaged library's app class loader.
    match unsafe { register_android_vm(vm) } {
        Ok(()) => JNI_VERSION_1_6,
        Err(_) => JNI_ERR,
    }
}

// Declared only by the instrumentation APK. No fixture or broker operation in UniFFI.
#[cfg(debug_assertions)]
#[unsafe(no_mangle)]
extern "system" fn Java_com_eon_assistant_NativeVaultAcceptance_checkFixture(
    _: jni::JNIEnv<'_>,
    _: jni::objects::JClass<'_>,
) -> jni::sys::jboolean {
    eon_foundation::credentials::check_android_vault_fixture().into()
}
