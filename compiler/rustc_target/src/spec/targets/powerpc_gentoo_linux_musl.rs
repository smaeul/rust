use crate::spec::Target;

pub fn target() -> Target {
    let mut base = super::powerpc_unknown_linux_musl::target();

    base.llvm_target = "powerpc-gentoo-linux-musl".into();
    base.vendor = "gentoo".into();
    base.options.crt_static_default = false;

    base
}
