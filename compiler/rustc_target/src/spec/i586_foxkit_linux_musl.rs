use crate::spec::Target;

pub fn target() -> Target {
    let mut base = super::i586_unknown_linux_musl::target();

    base.llvm_target = "i586-foxkit-linux-musl".to_string();
    base.vendor = "foxkit".to_string();
    base.options.crt_static_default = false;

    base
}
