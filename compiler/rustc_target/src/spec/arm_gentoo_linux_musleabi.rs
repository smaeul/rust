use crate::spec::Target;

pub fn target() -> Target {
    let mut base = super::arm_unknown_linux_musleabi::target();

    base.llvm_target = "arm-gentoo-linux-musleabi".to_string();
    base.vendor = "gentoo".to_string();
    base.options.crt_static_default = false;

    base
}
