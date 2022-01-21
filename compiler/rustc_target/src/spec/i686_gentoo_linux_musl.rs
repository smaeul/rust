use crate::spec::Target;

pub fn target() -> Target {
    let mut base = super::i686_unknown_linux_musl::target();

    base.llvm_target = "i686-gentoo-linux-musl".to_string();
    base.vendor = "gentoo".to_string();
    base.options.crt_static_default = false;

    base
}
