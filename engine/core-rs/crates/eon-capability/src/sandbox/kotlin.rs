//! The digest covers the worker, not a general purpose JVM launcher, and every
//! JAR, module and native JVM library it can open. No directory grants.
use super::*;
use sha2::{Digest, Sha256};
use std::os::fd::AsRawFd;

#[cfg(test)]
mod tests;

const IMAGE: &[&str] = &[
    "kotlin-language-server", "worker.jar", "kotlin-compiler.jar", "kotlin-stdlib.jar",
    "kotlin-reflect.jar", "kotlinx-coroutines-core-jvm.jar", "jre/lib/modules",
    "jre/lib/server/libjvm.so", "jre/lib/libjava.so", "jre/lib/libjimage.so",
    "jre/lib/libzip.so", "jre/lib/libnet.so", "jre/lib/libnio.so", "jre/lib/libverify.so",
];

const FWCD_IMAGE: &[&str] = &[
    "fwcd-kotlin-language-server", "worker.jar", "kotlin-stdlib.jar", "java-base.jar",
    "server-1.3.13.jar", "shared-1.3.13.jar", "kotlin-compiler-2.1.0.jar", "kotlin-reflect-2.1.0.jar",
    "kotlinx-coroutines-core-jvm-1.6.4.jar", "trove4j-1.0.20200330.jar", "annotations-13.0.jar",
    "org.eclipse.lsp4j-0.21.2.jar", "org.eclipse.lsp4j.jsonrpc-0.21.2.jar", "gson-2.10.1.jar",
    "jre/lib/modules", "jre/lib/server/libjvm.so", "jre/lib/libjava.so", "jre/lib/libjimage.so",
    "jre/lib/libzip.so", "jre/lib/libnet.so", "jre/lib/libnio.so", "jre/lib/libverify.so",
    "jre/conf/security/java.security",
];

pub(super) fn jvm_filter(rules: &mut BTreeMap<i64, Vec<seccompiler::SeccompRule>>) -> EonResult<()> {
    use seccompiler::{SeccompCmpArgLen::{Dword, Qword}, SeccompCmpOp::Eq, SeccompCondition, SeccompRule};
    // libnet aborts during JVM startup unless it can query its *own* FD cap.
    // Never change a limit, inspect another process, or add a socket operation.
    let conditions = [(0, Dword, 0), (1, Dword, libc::RLIMIT_NOFILE.into()), (2, Qword, 0)]
        .into_iter().map(|(i, width, value)| SeccompCondition::new(i, width, Eq, value))
        .collect::<Result<Vec<_>, _>>().and_then(SeccompRule::new)
        .map_err(|e| sandbox_err("Kotlin FD cap", e))?;
    rules.insert(libc::SYS_prlimit64, vec![conditions]);
    // Only stack a more restrictive filter (the launcher's no-exec seal).
    let seal = [(0, libc::SECCOMP_SET_MODE_FILTER as u64), (1, 0)]
        .into_iter().map(|(i, value)| SeccompCondition::new(i, Dword, Eq, value))
        .collect::<Result<Vec<_>, _>>().and_then(SeccompRule::new)
        .map_err(|e| sandbox_err("Kotlin no-exec seal", e))?;
    rules.insert(libc::SYS_seccomp, vec![seal]);
    Ok(())
}

pub(super) fn ruleset(
    mut created: RulesetCreated,
    program: &Path,
    read_only: &[PathBuf],
    workspace: &Path,
    digest: &[u8; 32],
    deadline: std::time::Instant,
) -> EonResult<RulesetCreated> {
    let err = |s| sandbox_err("Kotlin image", s);
    let image = if program.file_name() == Some(FWCD_IMAGE[0].as_ref()) { FWCD_IMAGE } else { IMAGE };
    if program.file_name() != Some(image[0].as_ref()) || read_only != [program] {
        return Err(err("unexpected executable or extra grant"));
    }
    let root = program.parent().ok_or_else(|| err("missing installation"))?;
    if root.starts_with(workspace) || workspace.starts_with(root) {
        return Err(err("installation must be disjoint from workspace"));
    }
    let mut manifest = Sha256::new();
    let mut remaining = 192_i64 << 20;
    for name in image {
        let path = root.join(name);
        if path.canonicalize().map_err(|e| sandbox_err("Kotlin image path", e))? != path {
            return Err(err("symlink in installation"));
        }
        let fd = PathFd::new(&path).map_err(|e| sandbox_err("Kotlin image fd", e))?;
        let meta = rustix::fs::fstat(&fd).map_err(|e| sandbox_err("Kotlin image metadata", e))?;
        if rustix::fs::FileType::from_raw_mode(meta.st_mode) != rustix::fs::FileType::RegularFile
            || !(1..=remaining.min(128 << 20)).contains(&meta.st_size) || meta.st_mode & 0o6222 != 0
            || !(meta.st_uid == 0 || meta.st_uid == rustix::process::geteuid().as_raw())
        {
            return Err(err("expected individual immutable operator-owned image files"));
        }
        let file = std::fs::File::open(format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd()))
            .map_err(|e| sandbox_err("Kotlin image file", e))?;
        remaining -= meta.st_size;
        use std::io::Read;
        let mut input = file.take(meta.st_size as u64 + 1);
        let mut read = 0;
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(err("digest verification timed out"));
            }
            let n = input.read(&mut buffer).map_err(|e| sandbox_err("Kotlin image digest", e))?;
            if n == 0 { break; }
            read += n;
            hash.update(&buffer[..n]);
        }
        if read as i64 != meta.st_size { return Err(err("image size changed during verification")); }
        manifest.update(format!("{:x}  {name}\n", hash.finalize()));
        let access = if *name == image[0] { AccessFs::ReadFile | AccessFs::Execute } else { AccessFs::ReadFile.into() };
        created = created.add_rule(PathBeneath::new(fd, access)).map_err(|e| sandbox_err("Kotlin image rule", e))?;
    }
    if manifest.finalize().as_slice() != digest {
        return Err(err("unapproved Kotlin image digest"));
    }
    // Only the ELF interpreter and the fixed system dependencies of this glibc image.
    for name in [
        "/lib64/ld-linux-x86-64.so.2", "/lib/x86_64-linux-gnu/libc.so.6",
        "/lib/x86_64-linux-gnu/libdl.so.2", "/lib/x86_64-linux-gnu/libpthread.so.0",
        "/lib/x86_64-linux-gnu/librt.so.1", "/lib/x86_64-linux-gnu/libm.so.6",
    ] {
        let path = Path::new(name).canonicalize().map_err(|e| sandbox_err("Kotlin libc path", e))?;
        if path.starts_with(workspace) {
            return Err(err("runtime must be disjoint from workspace"));
        }
        let fd = PathFd::new(&path).map_err(|e| sandbox_err("Kotlin libc fd", e))?;
        let file = std::fs::File::from(fd.as_fd().try_clone_to_owned().map_err(|e| sandbox_err("Kotlin libc fd", e))?);
        let meta = file.metadata().map_err(|e| sandbox_err("Kotlin libc metadata", e))?;
        let exec = rust_analyzer::executable_grant(&path, program, &fd, &meta).map_err(|e| sandbox_err("Kotlin libc", e))?;
        let access = if exec { AccessFs::Execute | AccessFs::ReadFile } else { AccessFs::ReadFile.into() };
        created = created.add_rule(PathBeneath::new(fd, access)).map_err(|e| sandbox_err("Kotlin libc rule", e))?;
    }
    Ok(created)
}
