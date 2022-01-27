use crate::spec::Target;

pub fn target() -> Target {
    let mut base = super::x86_64_unknown_linux_musl::target();

    base.llvm_target = "x86_64-gentoo-linux-musl".to_string();
    base.vendor = "gentoo".to_string();
    base.options.crt_static_default = false;

    base
}
