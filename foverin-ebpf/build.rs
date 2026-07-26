use std::{fs, path::PathBuf, process::Command};

use which::which;

/// Building this crate has an undeclared dependency on the `bpf-linker` binary. This would be
/// better expressed by [artifact-dependencies][bindeps] but issues such as
/// https://github.com/rust-lang/cargo/issues/12385 make their use impractical for the time being.
///
/// This file implements an imperfect solution: it causes cargo to rebuild the crate whenever the
/// mtime of `which bpf-linker` changes. Note that possibility that a new bpf-linker is added to
/// $PATH ahead of the one used as the cache key still exists. Solving this in the general case
/// would require rebuild-if-changed-env=PATH *and* rebuild-if-changed={every-directory-in-PATH}
/// which would likely mean far too much cache invalidation.
///
/// [bindeps]: https://doc.rust-lang.org/nightly/cargo/reference/unstable.html?highlight=feature#artifact-dependencies
fn main() {
    println!("cargo:rerun-if-env-changed=FOVERIN_SKIP_EBPF");
    // Userspace CI sets this so GitHub runners can check Rust without bpf-linker.
    if std::env::var_os("FOVERIN_SKIP_EBPF").is_some() {
        println!("cargo:warning=FOVERIN_SKIP_EBPF set — skipping bpf-linker probe");
        write_task_offsets_fallback();
        return;
    }

    let bpf_linker = which("bpf-linker").expect(
        "bpf-linker not found in PATH (install with `cargo install bpf-linker`, \
         or set FOVERIN_SKIP_EBPF=1 for userspace-only checks)",
    );
    println!("cargo:rerun-if-changed={}", bpf_linker.to_str().unwrap());

    generate_task_offsets();
}

/// Emit `task_offsets.rs` with `task_struct` field offsets for parent lineage.
///
/// Aya does not yet apply BPF CO-RE relocations for Rust field access, so we bake
/// offsets from the build host's BTF (`pahole` on `/sys/kernel/btf/vmlinux`).
/// Rebuild on the target kernel when deploying across distros/kernels.
fn generate_task_offsets() {
    println!("cargo:rerun-if-changed=/sys/kernel/btf/vmlinux");

    let btf = PathBuf::from("/sys/kernel/btf/vmlinux");
    if !btf.is_file() {
        println!(
            "cargo:warning=/sys/kernel/btf/vmlinux missing — using fallback task_struct offsets"
        );
        write_task_offsets_fallback();
        return;
    }

    let pahole = match which("pahole") {
        Ok(p) => p,
        Err(_) => {
            println!("cargo:warning=pahole not found — using fallback task_struct offsets");
            write_task_offsets_fallback();
            return;
        }
    };

    let output = Command::new(&pahole)
        .args(["-C", "task_struct"])
        .arg(&btf)
        .output();

    let Ok(output) = output else {
        println!("cargo:warning=pahole failed to run — using fallback task_struct offsets");
        write_task_offsets_fallback();
        return;
    };

    if !output.status.success() {
        println!("cargo:warning=pahole exited non-zero — using fallback task_struct offsets");
        write_task_offsets_fallback();
        return;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let Some(real_parent) = parse_pahole_offset(&text, "real_parent") else {
        println!("cargo:warning=could not parse real_parent offset — using fallback");
        write_task_offsets_fallback();
        return;
    };
    let Some(comm) = parse_pahole_offset(&text, "comm") else {
        println!("cargo:warning=could not parse comm offset — using fallback");
        write_task_offsets_fallback();
        return;
    };

    write_task_offsets(real_parent, comm);
}

/// pahole lines look like: `struct task_struct *       real_parent;          /*  2880     8 */`
/// or: `char                       comm[16];             /*  3400    16 */`
fn parse_pahole_offset(text: &str, field: &str) -> Option<usize> {
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(name_part) = trimmed.split(';').next() else {
            continue;
        };
        let Some(ident) = name_part.split_whitespace().last() else {
            continue;
        };
        // Strip C array suffix: `comm[16]` → `comm`
        let ident = ident.split('[').next().unwrap_or(ident);
        if ident != field {
            continue;
        }
        // Comment: /*  OFFSET  SIZE */
        let start = trimmed.find("/*")?;
        let body = trimmed.get(start + 2..)?.trim();
        let offset_str = body.split_whitespace().next()?;
        return offset_str.parse().ok();
    }
    None
}

fn write_task_offsets(real_parent: usize, comm: usize) {
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("task_offsets.rs");
    let contents = format!(
        "/// Byte offset of `task_struct::real_parent` (from build-host BTF).\n\
         pub const TASK_REAL_PARENT_OFFSET: usize = {real_parent};\n\
         /// Byte offset of `task_struct::comm` (from build-host BTF).\n\
         pub const TASK_COMM_OFFSET: usize = {comm};\n"
    );
    fs::write(&out, contents).expect("write task_offsets.rs");
}

fn write_task_offsets_fallback() {
    // CachyOS 7.1.x / recent mainline x86_64 snapshot — regenerate via pahole when possible.
    write_task_offsets(2880, 3400);
}
