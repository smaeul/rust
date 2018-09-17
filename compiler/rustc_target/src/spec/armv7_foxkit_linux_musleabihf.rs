use crate::spec::TargetResult;

pub fn target() -> TargetResult {
    let mut base = super::armv7_unknown_linux_musleabihf::target()?;

    base.llvm_target = "armv7-foxkit-linux-musleabihf".to_string();
    base.target_vendor = "foxkit".to_string();
    base.options.crt_static_default = false;

    Ok(base)
}
