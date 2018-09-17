use crate::spec::Target;

pub fn target() -> Target {
    let mut base = super::armv7_unknown_linux_musleabihf::target();

    base.llvm_target = "armv7-foxkit-linux-musleabihf".to_string();
    base.vendor = "foxkit".to_string();
    base.options.crt_static_default = false;

    base
}
