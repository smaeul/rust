use crate::spec::Target;

pub fn target() -> Target {
    let mut base = super::arm_unknown_linux_musleabi::target();

    base.llvm_target = "arm-gentoo-linux-musleabi".into();
    base.vendor = "gentoo".into();
    base.options.crt_static_default = false;

    base
}
