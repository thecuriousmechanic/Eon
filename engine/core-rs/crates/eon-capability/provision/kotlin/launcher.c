/* The confined executable is not java (or kotlinc): there is no general
 * command-line entry point. Rust sets cwd to the vetted image directory. */
#define _GNU_SOURCE
#include <jni.h>
#include <dlfcn.h>
#include <errno.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <pthread.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef EON_JAVA_CLASSPATH
#define EON_JAVA_CLASSPATH "worker.jar:kotlin-compiler.jar:kotlin-stdlib.jar:kotlin-reflect.jar:kotlinx-coroutines-core-jvm.jar"
#endif

static int seal_exec(void) {
    struct sock_filter code[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_execve, 1, 0),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_execveat, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog program = {sizeof(code)/sizeof(code[0]), code};
    // no_new_privs and the architecture-checked outer filter already apply.
    if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &program)) return 1;
    // A null pathname would yield EFAULT without the additional filter.
    return syscall(SYS_execve, NULL, NULL, NULL) != -1 || errno != EPERM;
}

static void *worker(void *unused) {
    (void)unused;
    JavaVMOption options[] = {
        {"-Djava.class.path=" EON_JAVA_CLASSPATH, NULL},
        {"-Djava.awt.headless=true", NULL}, {"-Duser.home=/eon-no-home", NULL},
        {"-Djava.io.tmpdir=/eon-no-scratch", NULL}, {"-Didea.home.path=/eon-no-idea", NULL},
        {"-Dfile.encoding=UTF-8", NULL}, {"-Duser.timezone=UTC", NULL},
        {"-XX:-UsePerfData", NULL}, {"-XX:+DisableAttachMechanism", NULL}, {"-Xrs", NULL},
        {"-XX:-UseContainerSupport", NULL}, {"-XX:ActiveProcessorCount=1", NULL},
        {"-XX:+UseSerialGC", NULL}, {"-XX:TieredStopAtLevel=1", NULL},
        {"-Xms16m", NULL}, {"-Xmx320m", NULL}, {"-Xss512k", NULL},
        {"-XX:MaxMetaspaceSize=160m", NULL}, {"-XX:ReservedCodeCacheSize=48m", NULL},
        {"-XX:CompressedClassSpaceSize=64m", NULL}, {"-Xshare:off", NULL},
    };
    JavaVM *vm;
    JNIEnv *env;
    JavaVMInitArgs args = {JNI_VERSION_1_8, sizeof(options)/sizeof(options[0]), options, JNI_FALSE};
    void *jvm = dlopen("./jre/lib/server/libjvm.so", RTLD_NOW | RTLD_GLOBAL);
    if (!jvm) return (void *)1;
    jint (*create)(JavaVM **, void **, void *) = dlsym(jvm, "JNI_CreateJavaVM");
    if (!create || create(&vm, (void **)&env, &args) != JNI_OK) return (void *)1;
    jclass worker = (*env)->FindClass(env, "eon/kotlin/Worker");
    jmethodID run = worker ? (*env)->GetStaticMethodID(env, worker, "run", "()V") : NULL;
    if (run) (*env)->CallStaticVoidMethod(env, worker, run);
    if ((*env)->ExceptionCheck(env)) {
        (*env)->ExceptionDescribe(env);
        return (void *)1;
    }
    return (void *)(intptr_t)(!run || (*vm)->DestroyJavaVM(vm) != JNI_OK);
}

int main(int argc, char **argv) {
    (void)argv;
    if (argc != 1 || seal_exec()) return 2;
    // HotSpot can determine a pthread's stack bounds without reading /proc.
    pthread_t thread;
    void *status;
    if (pthread_create(&thread, NULL, worker, NULL) || pthread_join(thread, &status)) return 1;
    return (int)(intptr_t)status;
}
