use crate::spec::TargetResult;

pub fn target() -> TargetResult {
    let mut base = super::powerpc_unknown_linux_musl::target()?;

    base.llvm_target = "powerpc-foxkit-linux-musl".to_string();
    base.target_vendor = "foxkit".to_string();
    base.options.crt_static_default = false;

    Ok(base)
}
