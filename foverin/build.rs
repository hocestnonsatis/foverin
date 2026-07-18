use anyhow::{Context as _, anyhow};
use aya_build::Toolchain;

fn main() -> anyhow::Result<()> {
    // CI / docs builds can skip the BPF object when llvm/bpf-linker are unavailable.
    // Daemon binaries produced this way are compile-checked only — do not ship them.
    println!("cargo:rerun-if-env-changed=FOVERIN_SKIP_EBPF");
    if std::env::var_os("FOVERIN_SKIP_EBPF").is_some() {
        let out = std::env::var("OUT_DIR").context("OUT_DIR")?;
        let stub = std::path::Path::new(&out).join("foverin-ebpf");
        std::fs::write(&stub, []).context("write eBPF stub")?;
        println!("cargo:warning=FOVERIN_SKIP_EBPF set — skipping eBPF object build (stub written)");
        return Ok(());
    }

    let cargo_metadata::Metadata { packages, .. } = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .context("MetadataCommand::exec")?;
    let ebpf_package = packages
        .into_iter()
        .find(|cargo_metadata::Package { name, .. }| name.as_str() == "foverin-ebpf")
        .ok_or_else(|| anyhow!("foverin-ebpf package not found"))?;
    let cargo_metadata::Package {
        name,
        manifest_path,
        ..
    } = ebpf_package;
    let ebpf_package = aya_build::Package {
        name: name.as_str(),
        root_dir: manifest_path
            .parent()
            .ok_or_else(|| anyhow!("no parent for {manifest_path}"))?
            .as_str(),
        ..Default::default()
    };
    aya_build::build_ebpf([ebpf_package], Toolchain::default())
}
