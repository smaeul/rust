use crate::spec::Target;

pub fn target() -> Target {
    let mut base = super::armv7_unknown_linux_musleabihf::target();

    base.llvm_target = "armv7a-gentoo-linux-musleabihf".into();
    base.vendor = "gentoo".into();
    base.options.crt_static_default = false;

    base
}
