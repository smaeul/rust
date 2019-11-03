use crate::spec::{LinkerFlavor, TargetOptions};

pub fn opts() -> TargetOptions {
    let mut base = super::linux_base::opts();

    base.env = "musl".into();

    // libssp_nonshared.a is needed for __stack_chk_fail_local when using libc.so
    base.post_link_args.insert(LinkerFlavor::Gcc, vec!["-lssp_nonshared".into()]);

    // These targets statically link libc by default
    base.crt_static_default = true;

    base
}
