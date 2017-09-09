use crate::spec::TargetOptions;

pub fn opts() -> TargetOptions {
    let mut base = super::linux_base::opts();

    base.env = "musl".into();

    // These targets statically link libc by default
    base.crt_static_default = true;

    base
}
