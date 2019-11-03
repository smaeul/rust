use crate::spec::{base, Cc, LinkerFlavor, Lld, TargetOptions};

pub fn opts() -> TargetOptions {
    let mut base = base::linux::opts();

    base.env = "musl".into();

    // libssp_nonshared.a is needed for __stack_chk_fail_local when using libc.so
    base.add_post_link_args(LinkerFlavor::Gnu(Cc::Yes, Lld::No), &["-lssp_nonshared"]);

    // These targets statically link libc by default
    base.crt_static_default = true;

    base
}
