use crate::spec::{base, TargetOptions};

pub fn opts() -> TargetOptions {
    let mut base = base::linux::opts();

    base.env = "musl".into();

    // These targets statically link libc by default
    base.crt_static_default = true;

    base
}
