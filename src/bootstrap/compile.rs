// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Implementation of compiling various phases of the compiler and standard
//! library.
//!
//! This module contains some of the real meat in the rustbuild build system
//! which is where Cargo is used to compiler the standard library, libtest, and
//! compiler. This module is also responsible for assembling the sysroot as it
//! goes along from the output of the previous stage.

use std::env;
use std::fs::{self, File};
use std::io::BufReader;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str;

use build_helper::{output, mtime, up_to_date};
use filetime::FileTime;
use rustc_serialize::json;

use channel::GitInfo;
use util::{exe, libdir, is_dylib, copy};
use {Build, Compiler, Mode};

/// Build the standard library.
///
/// This will build the standard library for a particular stage of the build
/// using the `compiler` targeting the `target` architecture. The artifacts
/// created will also be linked into the sysroot directory.
pub fn std(build: &Build, target: &str, compiler: &Compiler) {
    let libdir = build.sysroot_libdir(compiler, target);
    t!(fs::create_dir_all(&libdir));

    let _folder = build.fold_output(|| format!("stage{}-std", compiler.stage));
    println!("Building stage{} std artifacts ({} -> {})", compiler.stage,
             compiler.host, target);

    let out_dir = build.cargo_out(compiler, Mode::Libstd, target);
    build.clear_if_dirty(&out_dir, &build.compiler_path(compiler));
    let mut cargo = build.cargo(compiler, Mode::Libstd, target, "build");
    let mut features = build.std_features();

    if let Ok(target) = env::var("MACOSX_STD_DEPLOYMENT_TARGET") {
        cargo.env("MACOSX_DEPLOYMENT_TARGET", target);
    }

    // When doing a local rebuild we tell cargo that we're stage1 rather than
    // stage0. This works fine if the local rust and being-built rust have the
    // same view of what the default allocator is, but fails otherwise. Since
    // we don't have a way to express an allocator preference yet, work
    // around the issue in the case of a local rebuild with jemalloc disabled.
    if compiler.stage == 0 && build.local_rebuild && !build.config.use_jemalloc {
        features.push_str(" force_alloc_system");
    }

    if compiler.stage != 0 && build.config.sanitizers {
        // This variable is used by the sanitizer runtime crates, e.g.
        // rustc_lsan, to build the sanitizer runtime from C code
        // When this variable is missing, those crates won't compile the C code,
        // so we don't set this variable during stage0 where llvm-config is
        // missing
        // We also only build the runtimes when --enable-sanitizers (or its
        // config.toml equivalent) is used
        cargo.env("LLVM_CONFIG", build.llvm_config(target));
    }
    cargo.arg("--features").arg(features)
         .arg("--manifest-path")
         .arg(build.src.join("src/libstd/Cargo.toml"));

    if let Some(target) = build.config.target_config.get(target) {
        if let Some(ref jemalloc) = target.jemalloc {
            cargo.env("JEMALLOC_OVERRIDE", jemalloc);
        }
    }

    run_cargo(build,
              &mut cargo,
              &libstd_stamp(build, &compiler, target));
}

/// Link all libstd rlibs/dylibs into the sysroot location.
///
/// Links those artifacts generated by `compiler` to a the `stage` compiler's
/// sysroot for the specified `host` and `target`.
///
/// Note that this assumes that `compiler` has already generated the libstd
/// libraries for `target`, and this method will find them in the relevant
/// output directory.
pub fn std_link(build: &Build,
                compiler: &Compiler,
                target_compiler: &Compiler,
                target: &str) {
    println!("Copying stage{} std from stage{} ({} -> {} / {})",
             target_compiler.stage,
             compiler.stage,
             compiler.host,
             target_compiler.host,
             target);
    let libdir = build.sysroot_libdir(target_compiler, target);
    add_to_sysroot(&libdir, &libstd_stamp(build, compiler, target));

    if build.config.sanitizers && compiler.stage != 0 && target == "x86_64-apple-darwin" {
        // The sanitizers are only built in stage1 or above, so the dylibs will
        // be missing in stage0 and causes panic. See the `std()` function above
        // for reason why the sanitizers are not built in stage0.
        copy_apple_sanitizer_dylibs(&build.native_dir(target), "osx", &libdir);
    }
}

fn copy_apple_sanitizer_dylibs(native_dir: &Path, platform: &str, into: &Path) {
    for &sanitizer in &["asan", "tsan"] {
        let filename = format!("libclang_rt.{}_{}_dynamic.dylib", sanitizer, platform);
        let mut src_path = native_dir.join(sanitizer);
        src_path.push("build");
        src_path.push("lib");
        src_path.push("darwin");
        src_path.push(&filename);
        copy(&src_path, &into.join(filename));
    }
}

/// Build and prepare startup objects like rsbegin.o and rsend.o
///
/// These are primarily used on Windows right now for linking executables/dlls.
/// They don't require any library support as they're just plain old object
/// files, so we just use the nightly snapshot compiler to always build them (as
/// no other compilers are guaranteed to be available).
pub fn build_startup_objects(build: &Build, for_compiler: &Compiler, target: &str) {
    if !target.contains("pc-windows-gnu") {
        return
    }

    let compiler = Compiler::new(0, &build.config.build);
    let compiler_path = build.compiler_path(&compiler);
    let src_dir = &build.src.join("src/rtstartup");
    let dst_dir = &build.native_dir(target).join("rtstartup");
    let sysroot_dir = &build.sysroot_libdir(for_compiler, target);
    t!(fs::create_dir_all(dst_dir));
    t!(fs::create_dir_all(sysroot_dir));

    for file in &["rsbegin", "rsend"] {
        let src_file = &src_dir.join(file.to_string() + ".rs");
        let dst_file = &dst_dir.join(file.to_string() + ".o");
        if !up_to_date(src_file, dst_file) {
            let mut cmd = Command::new(&compiler_path);
            build.run(cmd.env("RUSTC_BOOTSTRAP", "1")
                        .arg("--cfg").arg(format!("stage{}", compiler.stage))
                        .arg("--target").arg(target)
                        .arg("--emit=obj")
                        .arg("--out-dir").arg(dst_dir)
                        .arg(src_file));
        }

        copy(dst_file, &sysroot_dir.join(file.to_string() + ".o"));
    }

    for obj in ["crt2.o", "dllcrt2.o"].iter() {
        copy(&compiler_file(build.cc(target), obj), &sysroot_dir.join(obj));
    }
}

/// Build libtest.
///
/// This will build libtest and supporting libraries for a particular stage of
/// the build using the `compiler` targeting the `target` architecture. The
/// artifacts created will also be linked into the sysroot directory.
pub fn test(build: &Build, target: &str, compiler: &Compiler) {
    let _folder = build.fold_output(|| format!("stage{}-test", compiler.stage));
    println!("Building stage{} test artifacts ({} -> {})", compiler.stage,
             compiler.host, target);
    let out_dir = build.cargo_out(compiler, Mode::Libtest, target);
    build.clear_if_dirty(&out_dir, &libstd_stamp(build, compiler, target));
    let mut cargo = build.cargo(compiler, Mode::Libtest, target, "build");
    if let Ok(target) = env::var("MACOSX_STD_DEPLOYMENT_TARGET") {
        cargo.env("MACOSX_DEPLOYMENT_TARGET", target);
    }
    cargo.arg("--manifest-path")
         .arg(build.src.join("src/libtest/Cargo.toml"));
    run_cargo(build,
              &mut cargo,
              &libtest_stamp(build, compiler, target));
}

/// Same as `std_link`, only for libtest
pub fn test_link(build: &Build,
                 compiler: &Compiler,
                 target_compiler: &Compiler,
                 target: &str) {
    println!("Copying stage{} test from stage{} ({} -> {} / {})",
             target_compiler.stage,
             compiler.stage,
             compiler.host,
             target_compiler.host,
             target);
    add_to_sysroot(&build.sysroot_libdir(target_compiler, target),
                   &libtest_stamp(build, compiler, target));
}

/// Build the compiler.
///
/// This will build the compiler for a particular stage of the build using
/// the `compiler` targeting the `target` architecture. The artifacts
/// created will also be linked into the sysroot directory.
pub fn rustc(build: &Build, target: &str, compiler: &Compiler) {
    let _folder = build.fold_output(|| format!("stage{}-rustc", compiler.stage));
    println!("Building stage{} compiler artifacts ({} -> {})",
             compiler.stage, compiler.host, target);

    let out_dir = build.cargo_out(compiler, Mode::Librustc, target);
    build.clear_if_dirty(&out_dir, &libtest_stamp(build, compiler, target));

    let mut cargo = build.cargo(compiler, Mode::Librustc, target, "build");
    cargo.arg("--features").arg(build.rustc_features())
         .arg("--manifest-path")
         .arg(build.src.join("src/rustc/Cargo.toml"));

    // Set some configuration variables picked up by build scripts and
    // the compiler alike
    cargo.env("CFG_RELEASE", build.rust_release())
         .env("CFG_RELEASE_CHANNEL", &build.config.channel)
         .env("CFG_VERSION", build.rust_version())
         .env("CFG_PREFIX", build.config.prefix.clone().unwrap_or(PathBuf::new()));

    if compiler.stage == 0 {
        cargo.env("CFG_LIBDIR_RELATIVE", "lib");
    } else {
        let libdir_relative = build.config.libdir_relative.clone().unwrap_or(PathBuf::from("lib"));
        cargo.env("CFG_LIBDIR_RELATIVE", libdir_relative);
    }

    // If we're not building a compiler with debugging information then remove
    // these two env vars which would be set otherwise.
    if build.config.rust_debuginfo_only_std {
        cargo.env_remove("RUSTC_DEBUGINFO");
        cargo.env_remove("RUSTC_DEBUGINFO_LINES");
    }

    if let Some(ref ver_date) = build.rust_info.commit_date() {
        cargo.env("CFG_VER_DATE", ver_date);
    }
    if let Some(ref ver_hash) = build.rust_info.sha() {
        cargo.env("CFG_VER_HASH", ver_hash);
    }
    if !build.unstable_features() {
        cargo.env("CFG_DISABLE_UNSTABLE_FEATURES", "1");
    }
    // Flag that rust llvm is in use
    if build.is_rust_llvm(target) {
        cargo.env("LLVM_RUSTLLVM", "1");
    }
    cargo.env("LLVM_CONFIG", build.llvm_config(target));
    let target_config = build.config.target_config.get(target);
    if let Some(s) = target_config.and_then(|c| c.llvm_config.as_ref()) {
        cargo.env("CFG_LLVM_ROOT", s);
    }
    // Building with a static libstdc++ is only supported on linux right now,
    // not for MSVC or macOS
    if build.config.llvm_static_stdcpp &&
       !target.contains("windows") &&
       !target.contains("apple") {
        cargo.env("LLVM_STATIC_STDCPP",
                  compiler_file(build.cxx(target), "libstdc++.a"));
    }
    if build.config.llvm_link_shared {
        cargo.env("LLVM_LINK_SHARED", "1");
    }
    if let Some(ref s) = build.config.rustc_default_linker {
        cargo.env("CFG_DEFAULT_LINKER", s);
    }
    if let Some(ref s) = build.config.rustc_default_ar {
        cargo.env("CFG_DEFAULT_AR", s);
    }
    run_cargo(build,
              &mut cargo,
              &librustc_stamp(build, compiler, target));
}

/// Same as `std_link`, only for librustc
pub fn rustc_link(build: &Build,
                  compiler: &Compiler,
                  target_compiler: &Compiler,
                  target: &str) {
    println!("Copying stage{} rustc from stage{} ({} -> {} / {})",
             target_compiler.stage,
             compiler.stage,
             compiler.host,
             target_compiler.host,
             target);
    add_to_sysroot(&build.sysroot_libdir(target_compiler, target),
                   &librustc_stamp(build, compiler, target));
}

/// Cargo's output path for the standard library in a given stage, compiled
/// by a particular compiler for the specified target.
fn libstd_stamp(build: &Build, compiler: &Compiler, target: &str) -> PathBuf {
    build.cargo_out(compiler, Mode::Libstd, target).join(".libstd.stamp")
}

/// Cargo's output path for libtest in a given stage, compiled by a particular
/// compiler for the specified target.
fn libtest_stamp(build: &Build, compiler: &Compiler, target: &str) -> PathBuf {
    build.cargo_out(compiler, Mode::Libtest, target).join(".libtest.stamp")
}

/// Cargo's output path for librustc in a given stage, compiled by a particular
/// compiler for the specified target.
fn librustc_stamp(build: &Build, compiler: &Compiler, target: &str) -> PathBuf {
    build.cargo_out(compiler, Mode::Librustc, target).join(".librustc.stamp")
}

fn compiler_file(compiler: &Path, file: &str) -> PathBuf {
    let out = output(Command::new(compiler)
                            .arg(format!("-print-file-name={}", file)));
    PathBuf::from(out.trim())
}

pub fn create_sysroot(build: &Build, compiler: &Compiler) {
    let sysroot = build.sysroot(compiler);
    let _ = fs::remove_dir_all(&sysroot);
    t!(fs::create_dir_all(&sysroot));
}

/// Prepare a new compiler from the artifacts in `stage`
///
/// This will assemble a compiler in `build/$host/stage$stage`. The compiler
/// must have been previously produced by the `stage - 1` build.config.build
/// compiler.
pub fn assemble_rustc(build: &Build, stage: u32, host: &str) {
    // nothing to do in stage0
    if stage == 0 {
        return
    }

    println!("Copying stage{} compiler ({})", stage, host);

    // The compiler that we're assembling
    let target_compiler = Compiler::new(stage, host);

    // The compiler that compiled the compiler we're assembling
    let build_compiler = Compiler::new(stage - 1, &build.config.build);

    // Link in all dylibs to the libdir
    let sysroot = build.sysroot(&target_compiler);
    let sysroot_libdir = sysroot.join(libdir(host));
    t!(fs::create_dir_all(&sysroot_libdir));
    let src_libdir = build.sysroot_libdir(&build_compiler, host);
    for f in t!(fs::read_dir(&src_libdir)).map(|f| t!(f)) {
        let filename = f.file_name().into_string().unwrap();
        if is_dylib(&filename) {
            copy(&f.path(), &sysroot_libdir.join(&filename));
        }
    }

    let out_dir = build.cargo_out(&build_compiler, Mode::Librustc, host);

    // Link the compiler binary itself into place
    let rustc = out_dir.join(exe("rustc", host));
    let bindir = sysroot.join("bin");
    t!(fs::create_dir_all(&bindir));
    let compiler = build.compiler_path(&Compiler::new(stage, host));
    let _ = fs::remove_file(&compiler);
    copy(&rustc, &compiler);

    // See if rustdoc exists to link it into place
    let rustdoc = exe("rustdoc", host);
    let rustdoc_src = out_dir.join(&rustdoc);
    let rustdoc_dst = bindir.join(&rustdoc);
    if fs::metadata(&rustdoc_src).is_ok() {
        let _ = fs::remove_file(&rustdoc_dst);
        copy(&rustdoc_src, &rustdoc_dst);
    }
}

/// Link some files into a rustc sysroot.
///
/// For a particular stage this will link the file listed in `stamp` into the
/// `sysroot_dst` provided.
fn add_to_sysroot(sysroot_dst: &Path, stamp: &Path) {
    t!(fs::create_dir_all(&sysroot_dst));
    let mut contents = Vec::new();
    t!(t!(File::open(stamp)).read_to_end(&mut contents));
    for part in contents.split(|b| *b == 0) {
        if part.is_empty() {
            continue
        }
        let path = Path::new(t!(str::from_utf8(part)));
        copy(&path, &sysroot_dst.join(path.file_name().unwrap()));
    }
}

/// Build a tool in `src/tools`
///
/// This will build the specified tool with the specified `host` compiler in
/// `stage` into the normal cargo output directory.
pub fn maybe_clean_tools(build: &Build, stage: u32, target: &str, mode: Mode) {
    let compiler = Compiler::new(stage, &build.config.build);

    let stamp = match mode {
        Mode::Libstd => libstd_stamp(build, &compiler, target),
        Mode::Libtest => libtest_stamp(build, &compiler, target),
        Mode::Librustc => librustc_stamp(build, &compiler, target),
        _ => panic!(),
    };
    let out_dir = build.cargo_out(&compiler, Mode::Tool, target);
    build.clear_if_dirty(&out_dir, &stamp);
}

/// Build a tool in `src/tools`
///
/// This will build the specified tool with the specified `host` compiler in
/// `stage` into the normal cargo output directory.
pub fn tool(build: &Build, stage: u32, target: &str, tool: &str) {
    let _folder = build.fold_output(|| format!("stage{}-{}", stage, tool));
    println!("Building stage{} tool {} ({})", stage, tool, target);

    let compiler = Compiler::new(stage, &build.config.build);

    let mut cargo = build.cargo(&compiler, Mode::Tool, target, "build");
    let dir = build.src.join("src/tools").join(tool);
    cargo.arg("--manifest-path").arg(dir.join("Cargo.toml"));

    // We don't want to build tools dynamically as they'll be running across
    // stages and such and it's just easier if they're not dynamically linked.
    cargo.env("RUSTC_NO_PREFER_DYNAMIC", "1");

    if let Some(dir) = build.openssl_install_dir(target) {
        cargo.env("OPENSSL_STATIC", "1");
        cargo.env("OPENSSL_DIR", dir);
        cargo.env("LIBZ_SYS_STATIC", "1");
    }

    cargo.env("CFG_RELEASE_CHANNEL", &build.config.channel);

    let info = GitInfo::new(&dir);
    if let Some(sha) = info.sha() {
        cargo.env("CFG_COMMIT_HASH", sha);
    }
    if let Some(sha_short) = info.sha_short() {
        cargo.env("CFG_SHORT_COMMIT_HASH", sha_short);
    }
    if let Some(date) = info.commit_date() {
        cargo.env("CFG_COMMIT_DATE", date);
    }

    build.run(&mut cargo);
}

fn run_cargo(build: &Build, cargo: &mut Command, stamp: &Path) {
    // Instruct Cargo to give us json messages on stdout, critically leaving
    // stderr as piped so we can get those pretty colors.
    cargo.arg("--message-format").arg("json")
         .stdout(Stdio::piped());
    build.verbose(&format!("running: {:?}", cargo));
    let mut child = match cargo.spawn() {
        Ok(child) => child,
        Err(e) => panic!("failed to execute command: {:?}\nerror: {}", cargo, e),
    };

    // `target_root_dir` looks like $dir/$target/release
    let target_root_dir = stamp.parent().unwrap();
    // `target_deps_dir` looks like $dir/$target/release/deps
    let target_deps_dir = target_root_dir.join("deps");
    // `host_root_dir` looks like $dir/release
    let host_root_dir = target_root_dir.parent().unwrap() // chop off `release`
                                       .parent().unwrap() // chop off `$target`
                                       .join(target_root_dir.file_name().unwrap());

    // Spawn Cargo slurping up its JSON output. We'll start building up the
    // `deps` array of all files it generated along with a `toplevel` array of
    // files we need to probe for later.
    let mut deps = Vec::new();
    let mut toplevel = Vec::new();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    for line in stdout.lines() {
        let line = t!(line);
        let json = if line.starts_with("{") {
            t!(line.parse::<json::Json>())
        } else {
            // If this was informational, just print it out and continue
            println!("{}", line);
            continue
        };
        if json.find("reason").and_then(|j| j.as_string()) != Some("compiler-artifact") {
            continue
        }
        for filename in json["filenames"].as_array().unwrap() {
            let filename = filename.as_string().unwrap();
            // Skip files like executables
            if !filename.ends_with(".rlib") &&
               !filename.ends_with(".lib") &&
               !is_dylib(&filename) {
                continue
            }

            let filename = Path::new(filename);

            // If this was an output file in the "host dir" we don't actually
            // worry about it, it's not relevant for us.
            if filename.starts_with(&host_root_dir) {
                continue

            // If this was output in the `deps` dir then this is a precise file
            // name (hash included) so we start tracking it.
            } else if filename.starts_with(&target_deps_dir) {
                deps.push(filename.to_path_buf());

            // Otherwise this was a "top level artifact" which right now doesn't
            // have a hash in the name, but there's a version of this file in
            // the `deps` folder which *does* have a hash in the name. That's
            // the one we'll want to we'll probe for it later.
            } else {
                toplevel.push((filename.file_stem().unwrap()
                                       .to_str().unwrap().to_string(),
                               filename.extension().unwrap().to_owned()
                                       .to_str().unwrap().to_string()));
            }
        }
    }

    // Make sure Cargo actually succeeded after we read all of its stdout.
    let status = t!(child.wait());
    if !status.success() {
        panic!("command did not execute successfully: {:?}\n\
                expected success, got: {}",
               cargo,
               status);
    }

    // Ok now we need to actually find all the files listed in `toplevel`. We've
    // got a list of prefix/extensions and we basically just need to find the
    // most recent file in the `deps` folder corresponding to each one.
    let contents = t!(target_deps_dir.read_dir())
        .map(|e| t!(e))
        .map(|e| (e.path(), e.file_name().into_string().unwrap(), t!(e.metadata())))
        .collect::<Vec<_>>();
    for (prefix, extension) in toplevel {
        let candidates = contents.iter().filter(|&&(_, ref filename, _)| {
            filename.starts_with(&prefix[..]) &&
                filename[prefix.len()..].starts_with("-") &&
                filename.ends_with(&extension[..])
        });
        let max = candidates.max_by_key(|&&(_, _, ref metadata)| {
            FileTime::from_last_modification_time(metadata)
        });
        let path_to_add = match max {
            Some(triple) => triple.0.to_str().unwrap(),
            None => panic!("no output generated for {:?} {:?}", prefix, extension),
        };
        if is_dylib(path_to_add) {
            let candidate = format!("{}.lib", path_to_add);
            let candidate = PathBuf::from(candidate);
            if candidate.exists() {
                deps.push(candidate);
            }
        }
        deps.push(path_to_add.into());
    }

    // Now we want to update the contents of the stamp file, if necessary. First
    // we read off the previous contents along with its mtime. If our new
    // contents (the list of files to copy) is different or if any dep's mtime
    // is newer then we rewrite the stamp file.
    deps.sort();
    let mut stamp_contents = Vec::new();
    if let Ok(mut f) = File::open(stamp) {
        t!(f.read_to_end(&mut stamp_contents));
    }
    let stamp_mtime = mtime(&stamp);
    let mut new_contents = Vec::new();
    let mut max = None;
    let mut max_path = None;
    for dep in deps {
        let mtime = mtime(&dep);
        if Some(mtime) > max {
            max = Some(mtime);
            max_path = Some(dep.clone());
        }
        new_contents.extend(dep.to_str().unwrap().as_bytes());
        new_contents.extend(b"\0");
    }
    let max = max.unwrap();
    let max_path = max_path.unwrap();
    if stamp_contents == new_contents && max <= stamp_mtime {
        return
    }
    if max > stamp_mtime {
        build.verbose(&format!("updating {:?} as {:?} changed", stamp, max_path));
    } else {
        build.verbose(&format!("updating {:?} as deps changed", stamp));
    }
    t!(t!(File::create(stamp)).write_all(&new_contents));
}
