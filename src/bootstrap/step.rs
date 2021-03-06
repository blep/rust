// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Definition of steps of the build system.
//!
//! This is where some of the real meat of rustbuild is located, in how we
//! define targets and the dependencies amongst them. This file can sort of be
//! viewed as just defining targets in a makefile which shell out to predefined
//! functions elsewhere about how to execute the target.
//!
//! The primary function here you're likely interested in is the `build_rules`
//! function. This will create a `Rules` structure which basically just lists
//! everything that rustbuild can do. Each rule has a human-readable name, a
//! path associated with it, some dependencies, and then a closure of how to
//! actually perform the rule.
//!
//! All steps below are defined in self-contained units, so adding a new target
//! to the build system should just involve adding the meta information here
//! along with the actual implementation elsewhere. You can find more comments
//! about how to define rules themselves below.

use std::collections::{BTreeMap, HashSet, HashMap};
use std::mem;

use check::{self, TestKind};
use compile;
use dist;
use doc;
use flags::Subcommand;
use install;
use native;
use {Compiler, Build, Mode};

pub fn run(build: &Build) {
    let rules = build_rules(build);
    let steps = rules.plan();
    rules.run(&steps);
}

pub fn build_rules<'a>(build: &'a Build) -> Rules {
    let mut rules = Rules::new(build);

    // This is the first rule that we're going to define for rustbuild, which is
    // used to compile LLVM itself. All rules are added through the `rules`
    // structure created above and are configured through a builder-style
    // interface.
    //
    // First up we see the `build` method. This represents a rule that's part of
    // the top-level `build` subcommand. For example `./x.py build` is what this
    // is associating with. Note that this is normally only relevant if you flag
    // a rule as `default`, which we'll talk about later.
    //
    // Next up we'll see two arguments to this method:
    //
    // * `llvm` - this is the "human readable" name of this target. This name is
    //            not accessed anywhere outside this file itself (e.g. not in
    //            the CLI nor elsewhere in rustbuild). The purpose of this is to
    //            easily define dependencies between rules. That is, other rules
    //            will depend on this with the name "llvm".
    // * `src/llvm` - this is the relevant path to the rule that we're working
    //                with. This path is the engine behind how commands like
    //                `./x.py build src/llvm` work. This should typically point
    //                to the relevant component, but if there's not really a
    //                path to be assigned here you can pass something like
    //                `path/to/nowhere` to ignore it.
    //
    // After we create the rule with the `build` method we can then configure
    // various aspects of it. For example this LLVM rule uses `.host(true)` to
    // flag that it's a rule only for host targets. In other words, LLVM isn't
    // compiled for targets configured through `--target` (e.g. those we're just
    // building a standard library for).
    //
    // Next up the `dep` method will add a dependency to this rule. The closure
    // is yielded the step that represents executing the `llvm` rule itself
    // (containing information like stage, host, target, ...) and then it must
    // return a target that the step depends on. Here LLVM is actually
    // interesting where a cross-compiled LLVM depends on the host LLVM, but
    // otherwise it has no dependencies.
    //
    // To handle this we do a bit of dynamic dispatch to see what the dependency
    // is. If we're building a LLVM for the build triple, then we don't actually
    // have any dependencies! To do that we return a dependency on the `Step::noop()`
    // target which does nothing.
    //
    // If we're build a cross-compiled LLVM, however, we need to assemble the
    // libraries from the previous compiler. This step has the same name as
    // ours (llvm) but we want it for a different target, so we use the
    // builder-style methods on `Step` to configure this target to the build
    // triple.
    //
    // Finally, to finish off this rule, we define how to actually execute it.
    // That logic is all defined in the `native` module so we just delegate to
    // the relevant function there. The argument to the closure passed to `run`
    // is a `Step` (defined below) which encapsulates information like the
    // stage, target, host, etc.
    rules.build("llvm", "src/llvm")
         .host(true)
         .dep(move |s| {
             if s.target == build.config.build {
                 Step::noop()
             } else {
                 s.target(&build.config.build)
             }
         })
         .run(move |s| native::llvm(build, s.target));

    // Ok! After that example rule  that's hopefully enough to explain what's
    // going on here. You can check out the API docs below and also see a bunch
    // more examples of rules directly below as well.

    // the compiler with no target libraries ready to go
    rules.build("rustc", "src/rustc")
         .dep(|s| s.name("create-sysroot").target(s.host))
         .dep(move |s| {
             if s.stage == 0 {
                 Step::noop()
             } else {
                 s.name("librustc")
                  .host(&build.config.build)
                  .stage(s.stage - 1)
             }
         })
         .run(move |s| compile::assemble_rustc(build, s.stage, s.target));

    // Helper for loading an entire DAG of crates, rooted at `name`
    let krates = |name: &str| {
        let mut ret = Vec::new();
        let mut list = vec![name];
        let mut visited = HashSet::new();
        while let Some(krate) = list.pop() {
            let default = krate == name;
            let krate = &build.crates[krate];
            let path = krate.path.strip_prefix(&build.src)
                // This handles out of tree paths
                .unwrap_or(&krate.path);
            ret.push((krate, path.to_str().unwrap(), default));
            for dep in krate.deps.iter() {
                if visited.insert(dep) && dep != "build_helper" {
                    list.push(dep);
                }
            }
        }
        return ret
    };

    // ========================================================================
    // Crate compilations
    //
    // Tools used during the build system but not shipped
    rules.build("create-sysroot", "path/to/nowhere")
         .run(move |s| compile::create_sysroot(build, &s.compiler()));

    // These rules are "pseudo rules" that don't actually do any work
    // themselves, but represent a complete sysroot with the relevant compiler
    // linked into place.
    //
    // That is, depending on "libstd" means that when the rule is completed then
    // the `stage` sysroot for the compiler `host` will be available with a
    // standard library built for `target` linked in place. Not all rules need
    // the compiler itself to be available, just the standard library, so
    // there's a distinction between the two.
    rules.build("libstd", "src/libstd")
         .dep(|s| s.name("rustc").target(s.host))
         .dep(|s| s.name("libstd-link"));
    rules.build("libtest", "src/libtest")
         .dep(|s| s.name("libstd"))
         .dep(|s| s.name("libtest-link"))
         .default(true);
    rules.build("librustc", "src/librustc")
         .dep(|s| s.name("libtest"))
         .dep(|s| s.name("librustc-link"))
         .host(true)
         .default(true);

    // Helper method to define the rules to link a crate into its place in the
    // sysroot.
    //
    // The logic here is a little subtle as there's a few cases to consider.
    // Not all combinations of (stage, host, target) actually require something
    // to be compiled, but rather libraries could get propagated from a
    // different location. For example:
    //
    // * Any crate with a `host` that's not the build triple will not actually
    //   compile something. A different `host` means that the build triple will
    //   actually compile the libraries, and then we'll copy them over from the
    //   build triple to the `host` directory.
    //
    // * Some crates aren't even compiled by the build triple, but may be copied
    //   from previous stages. For example if we're not doing a full bootstrap
    //   then we may just depend on the stage1 versions of libraries to be
    //   available to get linked forward.
    //
    // * Finally, there are some cases, however, which do indeed comiple crates
    //   and link them into place afterwards.
    //
    // The rule definition below mirrors these three cases. The `dep` method
    // calculates the correct dependency which either comes from stage1, a
    // different compiler, or from actually building the crate itself (the `dep`
    // rule). The `run` rule then mirrors these three cases and links the cases
    // forward into the compiler sysroot specified from the correct location.
    fn crate_rule<'a, 'b>(build: &'a Build,
                          rules: &'b mut Rules<'a>,
                          krate: &'a str,
                          dep: &'a str,
                          link: fn(&Build, &Compiler, &Compiler, &str))
                          -> RuleBuilder<'a, 'b> {
        let mut rule = rules.build(&krate, "path/to/nowhere");
        rule.dep(move |s| {
                if build.force_use_stage1(&s.compiler(), s.target) {
                    s.host(&build.config.build).stage(1)
                } else if s.host == build.config.build {
                    s.name(dep)
                } else {
                    s.host(&build.config.build)
                }
            })
            .run(move |s| {
                if build.force_use_stage1(&s.compiler(), s.target) {
                    link(build,
                         &s.stage(1).host(&build.config.build).compiler(),
                         &s.compiler(),
                         s.target)
                } else if s.host == build.config.build {
                    link(build, &s.compiler(), &s.compiler(), s.target)
                } else {
                    link(build,
                         &s.host(&build.config.build).compiler(),
                         &s.compiler(),
                         s.target)
                }
            });
            return rule
    }

    // Similar to the `libstd`, `libtest`, and `librustc` rules above, except
    // these rules only represent the libraries being available in the sysroot,
    // not the compiler itself. This is done as not all rules need a compiler in
    // the sysroot, but may just need the libraries.
    //
    // All of these rules use the helper definition above.
    crate_rule(build,
               &mut rules,
               "libstd-link",
               "build-crate-std",
               compile::std_link)
        .dep(|s| s.name("startup-objects"))
        .dep(|s| s.name("create-sysroot").target(s.host));
    crate_rule(build,
               &mut rules,
               "libtest-link",
               "build-crate-test",
               compile::test_link)
        .dep(|s| s.name("libstd-link"));
    crate_rule(build,
               &mut rules,
               "librustc-link",
               "build-crate-rustc-main",
               compile::rustc_link)
        .dep(|s| s.name("libtest-link"));

    for (krate, path, _default) in krates("std") {
        rules.build(&krate.build_step, path)
             .dep(|s| s.name("startup-objects"))
             .dep(move |s| s.name("rustc").host(&build.config.build).target(s.host))
             .run(move |s| compile::std(build, s.target, &s.compiler()));
    }
    for (krate, path, _default) in krates("test") {
        rules.build(&krate.build_step, path)
             .dep(|s| s.name("libstd-link"))
             .run(move |s| compile::test(build, s.target, &s.compiler()));
    }
    for (krate, path, _default) in krates("rustc-main") {
        rules.build(&krate.build_step, path)
             .dep(|s| s.name("libtest-link"))
             .dep(move |s| s.name("llvm").host(&build.config.build).stage(0))
             .dep(|s| s.name("may-run-build-script"))
             .run(move |s| compile::rustc(build, s.target, &s.compiler()));
    }

    // Crates which have build scripts need to rely on this rule to ensure that
    // the necessary prerequisites for a build script are linked and located in
    // place.
    rules.build("may-run-build-script", "path/to/nowhere")
         .dep(move |s| {
             s.name("libstd-link")
              .host(&build.config.build)
              .target(&build.config.build)
         });
    rules.build("startup-objects", "src/rtstartup")
         .dep(|s| s.name("create-sysroot").target(s.host))
         .run(move |s| compile::build_startup_objects(build, &s.compiler(), s.target));

    // ========================================================================
    // Test targets
    //
    // Various unit tests and tests suites we can run
    {
        let mut suite = |name, path, mode, dir| {
            rules.test(name, path)
                 .dep(|s| s.name("libtest"))
                 .dep(|s| s.name("tool-compiletest").target(s.host).stage(0))
                 .dep(|s| s.name("test-helpers"))
                 .dep(|s| s.name("remote-copy-libs"))
                 .default(mode != "pretty") // pretty tests don't run everywhere
                 .run(move |s| {
                     check::compiletest(build, &s.compiler(), s.target, mode, dir)
                 });
        };

        suite("check-ui", "src/test/ui", "ui", "ui");
        suite("check-rpass", "src/test/run-pass", "run-pass", "run-pass");
        suite("check-cfail", "src/test/compile-fail", "compile-fail", "compile-fail");
        suite("check-pfail", "src/test/parse-fail", "parse-fail", "parse-fail");
        suite("check-rfail", "src/test/run-fail", "run-fail", "run-fail");
        suite("check-rpass-valgrind", "src/test/run-pass-valgrind",
              "run-pass-valgrind", "run-pass-valgrind");
        suite("check-mir-opt", "src/test/mir-opt", "mir-opt", "mir-opt");
        if build.config.codegen_tests {
            suite("check-codegen", "src/test/codegen", "codegen", "codegen");
        }
        suite("check-codegen-units", "src/test/codegen-units", "codegen-units",
              "codegen-units");
        suite("check-incremental", "src/test/incremental", "incremental",
              "incremental");
    }

    if build.config.build.contains("msvc") {
        // nothing to do for debuginfo tests
    } else {
        rules.test("check-debuginfo-lldb", "src/test/debuginfo-lldb")
             .dep(|s| s.name("libtest"))
             .dep(|s| s.name("tool-compiletest").target(s.host).stage(0))
             .dep(|s| s.name("test-helpers"))
             .dep(|s| s.name("debugger-scripts"))
             .run(move |s| check::compiletest(build, &s.compiler(), s.target,
                                         "debuginfo-lldb", "debuginfo"));
        rules.test("check-debuginfo-gdb", "src/test/debuginfo-gdb")
             .dep(|s| s.name("libtest"))
             .dep(|s| s.name("tool-compiletest").target(s.host).stage(0))
             .dep(|s| s.name("test-helpers"))
             .dep(|s| s.name("debugger-scripts"))
             .dep(|s| s.name("remote-copy-libs"))
             .run(move |s| check::compiletest(build, &s.compiler(), s.target,
                                         "debuginfo-gdb", "debuginfo"));
        let mut rule = rules.test("check-debuginfo", "src/test/debuginfo");
        rule.default(true);
        if build.config.build.contains("apple") {
            rule.dep(|s| s.name("check-debuginfo-lldb"));
        } else {
            rule.dep(|s| s.name("check-debuginfo-gdb"));
        }
    }

    rules.test("debugger-scripts", "src/etc/lldb_batchmode.py")
         .run(move |s| dist::debugger_scripts(build, &build.sysroot(&s.compiler()),
                                         s.target));

    {
        let mut suite = |name, path, mode, dir| {
            rules.test(name, path)
                 .dep(|s| s.name("librustc"))
                 .dep(|s| s.name("test-helpers"))
                 .dep(|s| s.name("tool-compiletest").target(s.host).stage(0))
                 .default(mode != "pretty")
                 .host(true)
                 .run(move |s| {
                     check::compiletest(build, &s.compiler(), s.target, mode, dir)
                 });
        };

        suite("check-ui-full", "src/test/ui-fulldeps", "ui", "ui-fulldeps");
        suite("check-rpass-full", "src/test/run-pass-fulldeps",
              "run-pass", "run-pass-fulldeps");
        suite("check-rfail-full", "src/test/run-fail-fulldeps",
              "run-fail", "run-fail-fulldeps");
        suite("check-cfail-full", "src/test/compile-fail-fulldeps",
              "compile-fail", "compile-fail-fulldeps");
        suite("check-rmake", "src/test/run-make", "run-make", "run-make");
        suite("check-rustdoc", "src/test/rustdoc", "rustdoc", "rustdoc");
        suite("check-pretty", "src/test/pretty", "pretty", "pretty");
        suite("check-pretty-rpass", "src/test/run-pass/pretty", "pretty",
              "run-pass");
        suite("check-pretty-rfail", "src/test/run-fail/pretty", "pretty",
              "run-fail");
        suite("check-pretty-valgrind", "src/test/run-pass-valgrind/pretty", "pretty",
              "run-pass-valgrind");
        suite("check-pretty-rpass-full", "src/test/run-pass-fulldeps/pretty",
              "pretty", "run-pass-fulldeps");
        suite("check-pretty-rfail-full", "src/test/run-fail-fulldeps/pretty",
              "pretty", "run-fail-fulldeps");
    }

    for (krate, path, _default) in krates("std") {
        rules.test(&krate.test_step, path)
             .dep(|s| s.name("libtest"))
             .dep(|s| s.name("remote-copy-libs"))
             .run(move |s| check::krate(build, &s.compiler(), s.target,
                                        Mode::Libstd, TestKind::Test,
                                        Some(&krate.name)));
    }
    rules.test("check-std-all", "path/to/nowhere")
         .dep(|s| s.name("libtest"))
         .dep(|s| s.name("remote-copy-libs"))
         .default(true)
         .run(move |s| check::krate(build, &s.compiler(), s.target,
                                    Mode::Libstd, TestKind::Test, None));

    // std benchmarks
    for (krate, path, _default) in krates("std") {
        rules.bench(&krate.bench_step, path)
             .dep(|s| s.name("libtest"))
             .dep(|s| s.name("remote-copy-libs"))
             .run(move |s| check::krate(build, &s.compiler(), s.target,
                                        Mode::Libstd, TestKind::Bench,
                                        Some(&krate.name)));
    }
    rules.bench("bench-std-all", "path/to/nowhere")
         .dep(|s| s.name("libtest"))
         .dep(|s| s.name("remote-copy-libs"))
         .default(true)
         .run(move |s| check::krate(build, &s.compiler(), s.target,
                                    Mode::Libstd, TestKind::Bench, None));

    for (krate, path, _default) in krates("test") {
        rules.test(&krate.test_step, path)
             .dep(|s| s.name("libtest"))
             .dep(|s| s.name("remote-copy-libs"))
             .run(move |s| check::krate(build, &s.compiler(), s.target,
                                        Mode::Libtest, TestKind::Test,
                                        Some(&krate.name)));
    }
    rules.test("check-test-all", "path/to/nowhere")
         .dep(|s| s.name("libtest"))
         .dep(|s| s.name("remote-copy-libs"))
         .default(true)
         .run(move |s| check::krate(build, &s.compiler(), s.target,
                                    Mode::Libtest, TestKind::Test, None));
    for (krate, path, _default) in krates("rustc-main") {
        rules.test(&krate.test_step, path)
             .dep(|s| s.name("librustc"))
             .dep(|s| s.name("remote-copy-libs"))
             .host(true)
             .run(move |s| check::krate(build, &s.compiler(), s.target,
                                        Mode::Librustc, TestKind::Test,
                                        Some(&krate.name)));
    }
    rules.test("check-rustc-all", "path/to/nowhere")
         .dep(|s| s.name("librustc"))
         .dep(|s| s.name("remote-copy-libs"))
         .default(true)
         .host(true)
         .run(move |s| check::krate(build, &s.compiler(), s.target,
                                    Mode::Librustc, TestKind::Test, None));

    rules.test("check-linkchecker", "src/tools/linkchecker")
         .dep(|s| s.name("tool-linkchecker").stage(0))
         .dep(|s| s.name("default:doc"))
         .default(true)
         .host(true)
         .run(move |s| check::linkcheck(build, s.target));
    rules.test("check-cargotest", "src/tools/cargotest")
         .dep(|s| s.name("tool-cargotest").stage(0))
         .dep(|s| s.name("librustc"))
         .host(true)
         .run(move |s| check::cargotest(build, s.stage, s.target));
    rules.test("check-cargo", "cargo")
         .dep(|s| s.name("tool-cargo"))
         .host(true)
         .run(move |s| check::cargo(build, s.stage, s.target));
    rules.test("check-tidy", "src/tools/tidy")
         .dep(|s| s.name("tool-tidy").stage(0))
         .default(true)
         .host(true)
         .only_build(true)
         .run(move |s| check::tidy(build, s.target));
    rules.test("check-error-index", "src/tools/error_index_generator")
         .dep(|s| s.name("libstd"))
         .dep(|s| s.name("tool-error-index").host(s.host).stage(0))
         .default(true)
         .host(true)
         .run(move |s| check::error_index(build, &s.compiler()));
    rules.test("check-docs", "src/doc")
         .dep(|s| s.name("libtest"))
         .default(true)
         .host(true)
         .run(move |s| check::docs(build, &s.compiler()));
    rules.test("check-distcheck", "distcheck")
         .dep(|s| s.name("dist-plain-source-tarball"))
         .dep(|s| s.name("dist-src"))
         .run(move |_| check::distcheck(build));

    rules.build("test-helpers", "src/rt/rust_test_helpers.c")
         .run(move |s| native::test_helpers(build, s.target));
    rules.build("openssl", "path/to/nowhere")
         .run(move |s| native::openssl(build, s.target));

    // Some test suites are run inside emulators or on remote devices, and most
    // of our test binaries are linked dynamically which means we need to ship
    // the standard library and such to the emulator ahead of time. This step
    // represents this and is a dependency of all test suites.
    //
    // Most of the time this step is a noop (the `check::emulator_copy_libs`
    // only does work if necessary). For some steps such as shipping data to
    // QEMU we have to build our own tools so we've got conditional dependencies
    // on those programs as well. Note that the remote test client is built for
    // the build target (us) and the server is built for the target.
    rules.test("remote-copy-libs", "path/to/nowhere")
         .dep(|s| s.name("libtest"))
         .dep(move |s| {
             if build.remote_tested(s.target) {
                s.name("tool-remote-test-client").target(s.host).stage(0)
             } else {
                 Step::noop()
             }
         })
         .dep(move |s| {
             if build.remote_tested(s.target) {
                s.name("tool-remote-test-server")
             } else {
                 Step::noop()
             }
         })
         .run(move |s| check::remote_copy_libs(build, &s.compiler(), s.target));

    rules.test("check-bootstrap", "src/bootstrap")
         .default(true)
         .host(true)
         .only_build(true)
         .run(move |_| check::bootstrap(build));

    // ========================================================================
    // Build tools
    //
    // Tools used during the build system but not shipped
    rules.build("tool-rustbook", "src/tools/rustbook")
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("librustc-tool"))
         .run(move |s| compile::tool(build, s.stage, s.target, "rustbook"));
    rules.build("tool-error-index", "src/tools/error_index_generator")
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("librustc-tool"))
         .run(move |s| compile::tool(build, s.stage, s.target, "error_index_generator"));
    rules.build("tool-tidy", "src/tools/tidy")
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("libstd-tool"))
         .run(move |s| compile::tool(build, s.stage, s.target, "tidy"));
    rules.build("tool-linkchecker", "src/tools/linkchecker")
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("libstd-tool"))
         .run(move |s| compile::tool(build, s.stage, s.target, "linkchecker"));
    rules.build("tool-cargotest", "src/tools/cargotest")
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("libstd-tool"))
         .run(move |s| compile::tool(build, s.stage, s.target, "cargotest"));
    rules.build("tool-compiletest", "src/tools/compiletest")
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("libtest-tool"))
         .run(move |s| compile::tool(build, s.stage, s.target, "compiletest"));
    rules.build("tool-build-manifest", "src/tools/build-manifest")
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("libstd-tool"))
         .run(move |s| compile::tool(build, s.stage, s.target, "build-manifest"));
    rules.build("tool-remote-test-server", "src/tools/remote-test-server")
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("libstd-tool"))
         .run(move |s| compile::tool(build, s.stage, s.target, "remote-test-server"));
    rules.build("tool-remote-test-client", "src/tools/remote-test-client")
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("libstd-tool"))
         .run(move |s| compile::tool(build, s.stage, s.target, "remote-test-client"));
    rules.build("tool-rust-installer", "src/tools/rust-installer")
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("libstd-tool"))
         .run(move |s| compile::tool(build, s.stage, s.target, "rust-installer"));
    rules.build("tool-cargo", "src/tools/cargo")
         .host(true)
         .default(build.config.extended)
         .dep(|s| s.name("maybe-clean-tools"))
         .dep(|s| s.name("libstd-tool"))
         .dep(|s| s.stage(0).host(s.target).name("openssl"))
         .dep(move |s| {
             // Cargo depends on procedural macros, which requires a full host
             // compiler to be available, so we need to depend on that.
             s.name("librustc-link")
              .target(&build.config.build)
              .host(&build.config.build)
         })
         .run(move |s| compile::tool(build, s.stage, s.target, "cargo"));
    rules.build("tool-rls", "src/tools/rls")
         .host(true)
         .default(build.config.extended)
         .dep(|s| s.name("librustc-tool"))
         .dep(|s| s.stage(0).host(s.target).name("openssl"))
         .dep(move |s| {
             // rls, like cargo, uses procedural macros
             s.name("librustc-link")
              .target(&build.config.build)
              .host(&build.config.build)
         })
         .run(move |s| compile::tool(build, s.stage, s.target, "rls"));

    // "pseudo rule" which represents completely cleaning out the tools dir in
    // one stage. This needs to happen whenever a dependency changes (e.g.
    // libstd, libtest, librustc) and all of the tool compilations above will
    // be sequenced after this rule.
    rules.build("maybe-clean-tools", "path/to/nowhere")
         .after("librustc-tool")
         .after("libtest-tool")
         .after("libstd-tool");

    rules.build("librustc-tool", "path/to/nowhere")
         .dep(|s| s.name("librustc"))
         .run(move |s| compile::maybe_clean_tools(build, s.stage, s.target, Mode::Librustc));
    rules.build("libtest-tool", "path/to/nowhere")
         .dep(|s| s.name("libtest"))
         .run(move |s| compile::maybe_clean_tools(build, s.stage, s.target, Mode::Libtest));
    rules.build("libstd-tool", "path/to/nowhere")
         .dep(|s| s.name("libstd"))
         .run(move |s| compile::maybe_clean_tools(build, s.stage, s.target, Mode::Libstd));

    // ========================================================================
    // Documentation targets
    rules.doc("doc-book", "src/doc/book")
         .dep(move |s| {
             s.name("tool-rustbook")
              .host(&build.config.build)
              .target(&build.config.build)
              .stage(0)
         })
         .default(build.config.docs)
         .run(move |s| doc::book(build, s.target, "book"));
    rules.doc("doc-nomicon", "src/doc/nomicon")
         .dep(move |s| {
             s.name("tool-rustbook")
              .host(&build.config.build)
              .target(&build.config.build)
              .stage(0)
         })
         .default(build.config.docs)
         .run(move |s| doc::rustbook(build, s.target, "nomicon"));
    rules.doc("doc-reference", "src/doc/reference")
         .dep(move |s| {
             s.name("tool-rustbook")
              .host(&build.config.build)
              .target(&build.config.build)
              .stage(0)
         })
         .default(build.config.docs)
         .run(move |s| doc::rustbook(build, s.target, "reference"));
    rules.doc("doc-unstable-book", "src/doc/unstable-book")
         .dep(move |s| {
             s.name("tool-rustbook")
              .host(&build.config.build)
              .target(&build.config.build)
              .stage(0)
         })
         .default(build.config.docs)
         .run(move |s| doc::rustbook(build, s.target, "unstable-book"));
    rules.doc("doc-standalone", "src/doc")
         .dep(move |s| {
             s.name("rustc")
              .host(&build.config.build)
              .target(&build.config.build)
              .stage(0)
         })
         .default(build.config.docs)
         .run(move |s| doc::standalone(build, s.target));
    rules.doc("doc-error-index", "src/tools/error_index_generator")
         .dep(move |s| s.name("tool-error-index").target(&build.config.build).stage(0))
         .dep(move |s| s.name("librustc-link"))
         .default(build.config.docs)
         .host(true)
         .run(move |s| doc::error_index(build, s.target));
    for (krate, path, default) in krates("std") {
        rules.doc(&krate.doc_step, path)
             .dep(|s| s.name("libstd-link"))
             .default(default && build.config.docs)
             .run(move |s| doc::std(build, s.stage, s.target));
    }
    for (krate, path, default) in krates("test") {
        rules.doc(&krate.doc_step, path)
             .dep(|s| s.name("libtest-link"))
             // Needed so rustdoc generates relative links to std.
             .dep(|s| s.name("doc-crate-std"))
             .default(default && build.config.compiler_docs)
             .run(move |s| doc::test(build, s.stage, s.target));
    }
    for (krate, path, default) in krates("rustc-main") {
        rules.doc(&krate.doc_step, path)
             .dep(|s| s.name("librustc-link"))
             // Needed so rustdoc generates relative links to std.
             .dep(|s| s.name("doc-crate-std"))
             .host(true)
             .default(default && build.config.docs)
             .run(move |s| doc::rustc(build, s.stage, s.target));
    }

    // ========================================================================
    // Distribution targets
    rules.dist("dist-rustc", "src/librustc")
         .dep(move |s| s.name("rustc").host(&build.config.build))
         .host(true)
         .only_host_build(true)
         .default(true)
         .dep(move |s| tool_rust_installer(build, s))
         .run(move |s| dist::rustc(build, s.stage, s.target));
    rules.dist("dist-std", "src/libstd")
         .dep(move |s| {
             // We want to package up as many target libraries as possible
             // for the `rust-std` package, so if this is a host target we
             // depend on librustc and otherwise we just depend on libtest.
             if build.config.host.iter().any(|t| t == s.target) {
                 s.name("librustc-link")
             } else {
                 s.name("libtest-link")
             }
         })
         .default(true)
         .only_host_build(true)
         .dep(move |s| tool_rust_installer(build, s))
         .run(move |s| dist::std(build, &s.compiler(), s.target));
    rules.dist("dist-mingw", "path/to/nowhere")
         .default(true)
         .only_host_build(true)
         .dep(move |s| tool_rust_installer(build, s))
         .run(move |s| {
             if s.target.contains("pc-windows-gnu") {
                 dist::mingw(build, s.target)
             }
         });
    rules.dist("dist-plain-source-tarball", "src")
         .default(build.config.rust_dist_src)
         .host(true)
         .only_build(true)
         .only_host_build(true)
         .dep(move |s| tool_rust_installer(build, s))
         .run(move |_| dist::plain_source_tarball(build));
    rules.dist("dist-src", "src")
         .default(true)
         .host(true)
         .only_build(true)
         .only_host_build(true)
         .dep(move |s| tool_rust_installer(build, s))
         .run(move |_| dist::rust_src(build));
    rules.dist("dist-docs", "src/doc")
         .default(true)
         .only_host_build(true)
         .dep(|s| s.name("default:doc"))
         .dep(move |s| tool_rust_installer(build, s))
         .run(move |s| dist::docs(build, s.stage, s.target));
    rules.dist("dist-analysis", "analysis")
         .default(build.config.extended)
         .dep(|s| s.name("dist-std"))
         .only_host_build(true)
         .dep(move |s| tool_rust_installer(build, s))
         .run(move |s| dist::analysis(build, &s.compiler(), s.target));
    rules.dist("dist-rls", "rls")
         .host(true)
         .only_host_build(true)
         .dep(|s| s.name("tool-rls"))
         .dep(move |s| tool_rust_installer(build, s))
         .run(move |s| dist::rls(build, s.stage, s.target));
    rules.dist("dist-cargo", "cargo")
         .host(true)
         .only_host_build(true)
         .dep(|s| s.name("tool-cargo"))
         .dep(move |s| tool_rust_installer(build, s))
         .run(move |s| dist::cargo(build, s.stage, s.target));
    rules.dist("dist-extended", "extended")
         .default(build.config.extended)
         .host(true)
         .only_host_build(true)
         .dep(|d| d.name("dist-std"))
         .dep(|d| d.name("dist-rustc"))
         .dep(|d| d.name("dist-mingw"))
         .dep(|d| d.name("dist-docs"))
         .dep(|d| d.name("dist-cargo"))
         .dep(|d| d.name("dist-rls"))
         .dep(|d| d.name("dist-analysis"))
         .dep(move |s| tool_rust_installer(build, s))
         .run(move |s| dist::extended(build, s.stage, s.target));

    rules.dist("dist-sign", "hash-and-sign")
         .host(true)
         .only_build(true)
         .only_host_build(true)
         .dep(move |s| s.name("tool-build-manifest").target(&build.config.build).stage(0))
         .run(move |_| dist::hash_and_sign(build));

    rules.install("install-docs", "src/doc")
         .default(build.config.docs)
         .only_host_build(true)
         .dep(|s| s.name("dist-docs"))
         .run(move |s| install::Installer::new(build).install_docs(s.stage, s.target));
    rules.install("install-std", "src/libstd")
         .default(true)
         .only_host_build(true)
         .dep(|s| s.name("dist-std"))
         .run(move |s| install::Installer::new(build).install_std(s.stage));
    rules.install("install-cargo", "cargo")
         .default(build.config.extended)
         .host(true)
         .only_host_build(true)
         .dep(|s| s.name("dist-cargo"))
         .run(move |s| install::Installer::new(build).install_cargo(s.stage, s.target));
    rules.install("install-rls", "rls")
         .default(build.config.extended)
         .host(true)
         .only_host_build(true)
         .dep(|s| s.name("dist-rls"))
         .run(move |s| install::Installer::new(build).install_rls(s.stage, s.target));
    rules.install("install-analysis", "analysis")
         .default(build.config.extended)
         .only_host_build(true)
         .dep(|s| s.name("dist-analysis"))
         .run(move |s| install::Installer::new(build).install_analysis(s.stage, s.target));
    rules.install("install-src", "src")
         .default(build.config.extended)
         .host(true)
         .only_build(true)
         .only_host_build(true)
         .dep(|s| s.name("dist-src"))
         .run(move |s| install::Installer::new(build).install_src(s.stage));
    rules.install("install-rustc", "src/librustc")
         .default(true)
         .host(true)
         .only_host_build(true)
         .dep(|s| s.name("dist-rustc"))
         .run(move |s| install::Installer::new(build).install_rustc(s.stage, s.target));

    rules.verify();
    return rules;

    /// Helper to depend on a stage0 build-only rust-installer tool.
    fn tool_rust_installer<'a>(build: &'a Build, step: &Step<'a>) -> Step<'a> {
        step.name("tool-rust-installer")
            .host(&build.config.build)
            .target(&build.config.build)
            .stage(0)
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
struct Step<'a> {
    /// Human readable name of the rule this step is executing. Possible names
    /// are all defined above in `build_rules`.
    name: &'a str,

    /// The stage this step is executing in. This is typically 0, 1, or 2.
    stage: u32,

    /// This step will likely involve a compiler, and the target that compiler
    /// itself is built for is called the host, this variable. Typically this is
    /// the target of the build machine itself.
    host: &'a str,

    /// The target that this step represents generating. If you're building a
    /// standard library for a new suite of targets, for example, this'll be set
    /// to those targets.
    target: &'a str,
}

impl<'a> Step<'a> {
    fn noop() -> Step<'a> {
        Step { name: "", stage: 0, host: "", target: "" }
    }

    /// Creates a new step which is the same as this, except has a new name.
    fn name(&self, name: &'a str) -> Step<'a> {
        Step { name: name, ..*self }
    }

    /// Creates a new step which is the same as this, except has a new stage.
    fn stage(&self, stage: u32) -> Step<'a> {
        Step { stage: stage, ..*self }
    }

    /// Creates a new step which is the same as this, except has a new host.
    fn host(&self, host: &'a str) -> Step<'a> {
        Step { host: host, ..*self }
    }

    /// Creates a new step which is the same as this, except has a new target.
    fn target(&self, target: &'a str) -> Step<'a> {
        Step { target: target, ..*self }
    }

    /// Returns the `Compiler` structure that this step corresponds to.
    fn compiler(&self) -> Compiler<'a> {
        Compiler::new(self.stage, self.host)
    }
}

struct Rule<'a> {
    /// The human readable name of this target, defined in `build_rules`.
    name: &'a str,

    /// The path associated with this target, used in the `./x.py` driver for
    /// easy and ergonomic specification of what to do.
    path: &'a str,

    /// The "kind" of top-level command that this rule is associated with, only
    /// relevant if this is a default rule.
    kind: Kind,

    /// List of dependencies this rule has. Each dependency is a function from a
    /// step that's being executed to another step that should be executed.
    deps: Vec<Box<Fn(&Step<'a>) -> Step<'a> + 'a>>,

    /// How to actually execute this rule. Takes a step with contextual
    /// information and then executes it.
    run: Box<Fn(&Step<'a>) + 'a>,

    /// Whether or not this is a "default" rule. That basically means that if
    /// you run, for example, `./x.py test` whether it's included or not.
    default: bool,

    /// Whether or not this is a "host" rule, or in other words whether this is
    /// only intended for compiler hosts and not for targets that are being
    /// generated.
    host: bool,

    /// Whether this rule is only for steps where the host is the build triple,
    /// not anything in hosts or targets.
    only_host_build: bool,

    /// Whether this rule is only for the build triple, not anything in hosts or
    /// targets.
    only_build: bool,

    /// A list of "order only" dependencies. This rules does not actually
    /// depend on these rules, but if they show up in the dependency graph then
    /// this rule must be executed after all these rules.
    after: Vec<&'a str>,
}

#[derive(PartialEq)]
enum Kind {
    Build,
    Test,
    Bench,
    Dist,
    Doc,
    Install,
}

impl<'a> Rule<'a> {
    fn new(name: &'a str, path: &'a str, kind: Kind) -> Rule<'a> {
        Rule {
            name: name,
            deps: Vec::new(),
            run: Box::new(|_| ()),
            path: path,
            kind: kind,
            default: false,
            host: false,
            only_host_build: false,
            only_build: false,
            after: Vec::new(),
        }
    }
}

/// Builder pattern returned from the various methods on `Rules` which will add
/// the rule to the internal list on `Drop`.
struct RuleBuilder<'a: 'b, 'b> {
    rules: &'b mut Rules<'a>,
    rule: Rule<'a>,
}

impl<'a, 'b> RuleBuilder<'a, 'b> {
    fn dep<F>(&mut self, f: F) -> &mut Self
        where F: Fn(&Step<'a>) -> Step<'a> + 'a,
    {
        self.rule.deps.push(Box::new(f));
        self
    }

    fn after(&mut self, step: &'a str) -> &mut Self {
        self.rule.after.push(step);
        self
    }

    fn run<F>(&mut self, f: F) -> &mut Self
        where F: Fn(&Step<'a>) + 'a,
    {
        self.rule.run = Box::new(f);
        self
    }

    fn default(&mut self, default: bool) -> &mut Self {
        self.rule.default = default;
        self
    }

    fn host(&mut self, host: bool) -> &mut Self {
        self.rule.host = host;
        self
    }

    fn only_build(&mut self, only_build: bool) -> &mut Self {
        self.rule.only_build = only_build;
        self
    }

    fn only_host_build(&mut self, only_host_build: bool) -> &mut Self {
        self.rule.only_host_build = only_host_build;
        self
    }
}

impl<'a, 'b> Drop for RuleBuilder<'a, 'b> {
    fn drop(&mut self) {
        let rule = mem::replace(&mut self.rule, Rule::new("", "", Kind::Build));
        let prev = self.rules.rules.insert(rule.name, rule);
        if let Some(prev) = prev {
            panic!("duplicate rule named: {}", prev.name);
        }
    }
}

pub struct Rules<'a> {
    build: &'a Build,
    sbuild: Step<'a>,
    rules: BTreeMap<&'a str, Rule<'a>>,
}

impl<'a> Rules<'a> {
    fn new(build: &'a Build) -> Rules<'a> {
        Rules {
            build: build,
            sbuild: Step {
                stage: build.flags.stage.unwrap_or(2),
                target: &build.config.build,
                host: &build.config.build,
                name: "",
            },
            rules: BTreeMap::new(),
        }
    }

    /// Creates a new rule of `Kind::Build` with the specified human readable
    /// name and path associated with it.
    ///
    /// The builder returned should be configured further with information such
    /// as how to actually run this rule.
    fn build<'b>(&'b mut self, name: &'a str, path: &'a str)
                 -> RuleBuilder<'a, 'b> {
        self.rule(name, path, Kind::Build)
    }

    /// Same as `build`, but for `Kind::Test`.
    fn test<'b>(&'b mut self, name: &'a str, path: &'a str)
                -> RuleBuilder<'a, 'b> {
        self.rule(name, path, Kind::Test)
    }

    /// Same as `build`, but for `Kind::Bench`.
    fn bench<'b>(&'b mut self, name: &'a str, path: &'a str)
                -> RuleBuilder<'a, 'b> {
        self.rule(name, path, Kind::Bench)
    }

    /// Same as `build`, but for `Kind::Doc`.
    fn doc<'b>(&'b mut self, name: &'a str, path: &'a str)
               -> RuleBuilder<'a, 'b> {
        self.rule(name, path, Kind::Doc)
    }

    /// Same as `build`, but for `Kind::Dist`.
    fn dist<'b>(&'b mut self, name: &'a str, path: &'a str)
                -> RuleBuilder<'a, 'b> {
        self.rule(name, path, Kind::Dist)
    }

    /// Same as `build`, but for `Kind::Install`.
    fn install<'b>(&'b mut self, name: &'a str, path: &'a str)
                -> RuleBuilder<'a, 'b> {
        self.rule(name, path, Kind::Install)
    }

    fn rule<'b>(&'b mut self,
                name: &'a str,
                path: &'a str,
                kind: Kind) -> RuleBuilder<'a, 'b> {
        RuleBuilder {
            rules: self,
            rule: Rule::new(name, path, kind),
        }
    }

    /// Verify the dependency graph defined by all our rules are correct, e.g.
    /// everything points to a valid something else.
    fn verify(&self) {
        for rule in self.rules.values() {
            for dep in rule.deps.iter() {
                let dep = dep(&self.sbuild.name(rule.name));
                if self.rules.contains_key(&dep.name) || dep.name.starts_with("default:") {
                    continue
                }
                if dep == Step::noop() {
                    continue
                }
                panic!("\

invalid rule dependency graph detected, was a rule added and maybe typo'd?

    `{}` depends on `{}` which does not exist

", rule.name, dep.name);
            }
        }
    }

    pub fn get_help(&self, command: &str) -> Option<String> {
        let kind = match command {
            "build" => Kind::Build,
            "doc" => Kind::Doc,
            "test" => Kind::Test,
            "bench" => Kind::Bench,
            "dist" => Kind::Dist,
            "install" => Kind::Install,
            _ => return None,
        };
        let rules = self.rules.values().filter(|r| r.kind == kind);
        let rules = rules.filter(|r| !r.path.contains("nowhere"));
        let mut rules = rules.collect::<Vec<_>>();
        rules.sort_by_key(|r| r.path);

        let mut help_string = String::from("Available paths:\n");
        for rule in rules {
            help_string.push_str(format!("    ./x.py {} {}\n", command, rule.path).as_str());
        }
        Some(help_string)
    }

    /// Construct the top-level build steps that we're going to be executing,
    /// given the subcommand that our build is performing.
    fn plan(&self) -> Vec<Step<'a>> {
        // Ok, the logic here is pretty subtle, and involves quite a few
        // conditionals. The basic idea here is to:
        //
        // 1. First, filter all our rules to the relevant ones. This means that
        //    the command specified corresponds to one of our `Kind` variants,
        //    and we filter all rules based on that.
        //
        // 2. Next, we determine which rules we're actually executing. If a
        //    number of path filters were specified on the command line we look
        //    for those, otherwise we look for anything tagged `default`.
        //    Here we also compute the priority of each rule based on how early
        //    in the command line the matching path filter showed up.
        //
        // 3. Finally, we generate some steps with host and target information.
        //
        // The last step is by far the most complicated and subtle. The basic
        // thinking here is that we want to take the cartesian product of
        // specified hosts and targets and build rules with that. The list of
        // hosts and targets, if not specified, come from the how this build was
        // configured. If the rule we're looking at is a host-only rule the we
        // ignore the list of targets and instead consider the list of hosts
        // also the list of targets.
        //
        // Once the host and target lists are generated we take the cartesian
        // product of the two and then create a step based off them. Note that
        // the stage each step is associated was specified with the `--step`
        // flag on the command line.
        let (kind, paths) = match self.build.flags.cmd {
            Subcommand::Build { ref paths } => (Kind::Build, &paths[..]),
            Subcommand::Doc { ref paths } => (Kind::Doc, &paths[..]),
            Subcommand::Test { ref paths, test_args: _ } => (Kind::Test, &paths[..]),
            Subcommand::Bench { ref paths, test_args: _ } => (Kind::Bench, &paths[..]),
            Subcommand::Dist { ref paths } => (Kind::Dist, &paths[..]),
            Subcommand::Install { ref paths } => (Kind::Install, &paths[..]),
            Subcommand::Clean => panic!(),
        };

        let mut rules: Vec<_> = self.rules.values().filter_map(|rule| {
            if rule.kind != kind {
                return None;
            }

            if paths.len() == 0 && rule.default {
                Some((rule, 0))
            } else {
                paths.iter().position(|path| path.ends_with(rule.path))
                     .map(|priority| (rule, priority))
            }
        }).collect();

        rules.sort_by_key(|&(_, priority)| priority);

        rules.into_iter().flat_map(|(rule, _)| {
            let hosts = if rule.only_host_build || rule.only_build {
                &self.build.config.host[..1]
            } else if self.build.flags.host.len() > 0 {
                &self.build.flags.host
            } else {
                &self.build.config.host
            };
            let targets = if self.build.flags.target.len() > 0 {
                &self.build.flags.target
            } else {
                &self.build.config.target
            };
            // Determine the actual targets participating in this rule.
            // NOTE: We should keep the full projection from build triple to
            // the hosts for the dist steps, now that the hosts array above is
            // truncated to avoid duplication of work in that case. Therefore
            // the original non-shadowed hosts array is used below.
            let arr = if rule.host {
                // If --target was specified but --host wasn't specified,
                // don't run any host-only tests. Also, respect any `--host`
                // overrides as done for `hosts`.
                if self.build.flags.host.len() > 0 {
                    &self.build.flags.host[..]
                } else if self.build.flags.target.len() > 0 {
                    &[]
                } else if rule.only_build {
                    &self.build.config.host[..1]
                } else {
                    &self.build.config.host[..]
                }
            } else {
                targets
            };

            hosts.iter().flat_map(move |host| {
                arr.iter().map(move |target| {
                    self.sbuild.name(rule.name).target(target).host(host)
                })
            })
        }).collect()
    }

    /// Execute all top-level targets indicated by `steps`.
    ///
    /// This will take the list returned by `plan` and then execute each step
    /// along with all required dependencies as it goes up the chain.
    fn run(&self, steps: &[Step<'a>]) {
        self.build.verbose("bootstrap top targets:");
        for step in steps.iter() {
            self.build.verbose(&format!("\t{:?}", step));
        }

        // Using `steps` as the top-level targets, make a topological ordering
        // of what we need to do.
        let order = self.expand(steps);

        // Print out what we're doing for debugging
        self.build.verbose("bootstrap build plan:");
        for step in order.iter() {
            self.build.verbose(&format!("\t{:?}", step));
        }

        // And finally, iterate over everything and execute it.
        for step in order.iter() {
            if self.build.flags.keep_stage.map_or(false, |s| step.stage <= s) {
                self.build.verbose(&format!("keeping step {:?}", step));
                continue;
            }
            self.build.verbose(&format!("executing step {:?}", step));
            (self.rules[step.name].run)(step);
        }
    }

    /// From the top level targets `steps` generate a topological ordering of
    /// all steps needed to run those steps.
    fn expand(&self, steps: &[Step<'a>]) -> Vec<Step<'a>> {
        // First up build a graph of steps and their dependencies. The `nodes`
        // map is a map from step to a unique number. The `edges` map is a
        // map from these unique numbers to a list of other numbers,
        // representing dependencies.
        let mut nodes = HashMap::new();
        nodes.insert(Step::noop(), 0);
        let mut edges = HashMap::new();
        edges.insert(0, HashSet::new());
        for step in steps {
            self.build_graph(step.clone(), &mut nodes, &mut edges);
        }

        // Now that we've built up the actual dependency graph, draw more
        // dependency edges to satisfy the `after` dependencies field for each
        // rule.
        self.satisfy_after_deps(&nodes, &mut edges);

        // And finally, perform a topological sort to return a list of steps to
        // execute.
        let mut order = Vec::new();
        let mut visited = HashSet::new();
        visited.insert(0);
        let idx_to_node = nodes.iter().map(|p| (*p.1, p.0)).collect::<HashMap<_, _>>();
        for idx in 0..nodes.len() {
            self.topo_sort(idx, &idx_to_node, &edges, &mut visited, &mut order);
        }
        return order
    }

    /// Builds the dependency graph rooted at `step`.
    ///
    /// The `nodes` and `edges` maps are filled out according to the rule
    /// described by `step.name`.
    fn build_graph(&self,
                   step: Step<'a>,
                   nodes: &mut HashMap<Step<'a>, usize>,
                   edges: &mut HashMap<usize, HashSet<usize>>) -> usize {
        use std::collections::hash_map::Entry;

        let idx = nodes.len();
        match nodes.entry(step.clone()) {
            Entry::Vacant(e) => { e.insert(idx); }
            Entry::Occupied(e) => return *e.get(),
        }

        let mut deps = Vec::new();
        for dep in self.rules[step.name].deps.iter() {
            let dep = dep(&step);
            if dep.name.starts_with("default:") {
                let kind = match &dep.name[8..] {
                    "doc" => Kind::Doc,
                    "dist" => Kind::Dist,
                    kind => panic!("unknown kind: `{}`", kind),
                };
                let host = self.build.config.host.iter().any(|h| h == dep.target);
                let rules = self.rules.values().filter(|r| r.default);
                for rule in rules.filter(|r| r.kind == kind && (!r.host || host)) {
                    deps.push(self.build_graph(dep.name(rule.name), nodes, edges));
                }
            } else {
                deps.push(self.build_graph(dep, nodes, edges));
            }
        }

        edges.entry(idx).or_insert(HashSet::new()).extend(deps);
        return idx
    }

    /// Given a dependency graph with a finished list of `nodes`, fill out more
    /// dependency `edges`.
    ///
    /// This is the step which satisfies all `after` listed dependencies in
    /// `Rule` above.
    fn satisfy_after_deps(&self,
                          nodes: &HashMap<Step<'a>, usize>,
                          edges: &mut HashMap<usize, HashSet<usize>>) {
        // Reverse map from the name of a step to the node indices that it
        // appears at.
        let mut name_to_idx = HashMap::new();
        for (step, &idx) in nodes {
            name_to_idx.entry(step.name).or_insert(Vec::new()).push(idx);
        }

        for (step, idx) in nodes {
            if *step == Step::noop() {
                continue
            }
            for after in self.rules[step.name].after.iter() {
                // This is the critical piece of an `after` dependency. If the
                // dependency isn't actually in our graph then no edge is drawn,
                // only if it's already present do we draw the edges.
                if let Some(idxs) = name_to_idx.get(after) {
                    edges.get_mut(idx).unwrap()
                         .extend(idxs.iter().cloned());
                }
            }
        }
    }

    fn topo_sort(&self,
                 cur: usize,
                 nodes: &HashMap<usize, &Step<'a>>,
                 edges: &HashMap<usize, HashSet<usize>>,
                 visited: &mut HashSet<usize>,
                 order: &mut Vec<Step<'a>>) {
        if !visited.insert(cur) {
            return
        }
        for dep in edges[&cur].iter() {
            self.topo_sort(*dep, nodes, edges, visited, order);
        }
        order.push(nodes[&cur].clone());
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use Build;
    use config::Config;
    use flags::Flags;

    fn build(args: &[&str],
             extra_host: &[&str],
             extra_target: &[&str]) -> Build {
        let mut args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        args.push("--build".to_string());
        args.push("A".to_string());
        let flags = Flags::parse(&args);

        let mut config = Config::default();
        config.docs = true;
        config.build = "A".to_string();
        config.host = vec![config.build.clone()];
        config.host.extend(extra_host.iter().map(|s| s.to_string()));
        config.target = config.host.clone();
        config.target.extend(extra_target.iter().map(|s| s.to_string()));

        let mut build = Build::new(flags, config);
        let cwd = env::current_dir().unwrap();
        build.crates.insert("std".to_string(), ::Crate {
            name: "std".to_string(),
            deps: Vec::new(),
            path: cwd.join("src/std"),
            doc_step: "doc-crate-std".to_string(),
            build_step: "build-crate-std".to_string(),
            test_step: "test-crate-std".to_string(),
            bench_step: "bench-crate-std".to_string(),
            version: String::new(),
        });
        build.crates.insert("test".to_string(), ::Crate {
            name: "test".to_string(),
            deps: Vec::new(),
            path: cwd.join("src/test"),
            doc_step: "doc-crate-test".to_string(),
            build_step: "build-crate-test".to_string(),
            test_step: "test-crate-test".to_string(),
            bench_step: "bench-crate-test".to_string(),
            version: String::new(),
        });
        build.crates.insert("rustc-main".to_string(), ::Crate {
            name: "rustc-main".to_string(),
            deps: Vec::new(),
            version: String::new(),
            path: cwd.join("src/rustc-main"),
            doc_step: "doc-crate-rustc-main".to_string(),
            build_step: "build-crate-rustc-main".to_string(),
            test_step: "test-crate-rustc-main".to_string(),
            bench_step: "bench-crate-rustc-main".to_string(),
        });
        return build
    }

    #[test]
    fn dist_baseline() {
        let build = build(&["dist"], &[], &[]);
        let rules = super::build_rules(&build);
        let plan = rules.plan();
        println!("rules: {:#?}", plan);
        assert!(plan.iter().all(|s| s.stage == 2));
        assert!(plan.iter().all(|s| s.host == "A" ));
        assert!(plan.iter().all(|s| s.target == "A" ));

        let step = super::Step {
            name: "",
            stage: 2,
            host: &build.config.build,
            target: &build.config.build,
        };

        assert!(plan.contains(&step.name("dist-docs")));
        assert!(plan.contains(&step.name("dist-mingw")));
        assert!(plan.contains(&step.name("dist-rustc")));
        assert!(plan.contains(&step.name("dist-std")));
        assert!(plan.contains(&step.name("dist-src")));
    }

    #[test]
    fn dist_with_targets() {
        let build = build(&["dist"], &[], &["B"]);
        let rules = super::build_rules(&build);
        let plan = rules.plan();
        println!("rules: {:#?}", plan);
        assert!(plan.iter().all(|s| s.stage == 2));
        assert!(plan.iter().all(|s| s.host == "A" ));

        let step = super::Step {
            name: "",
            stage: 2,
            host: &build.config.build,
            target: &build.config.build,
        };

        assert!(plan.contains(&step.name("dist-docs")));
        assert!(plan.contains(&step.name("dist-mingw")));
        assert!(plan.contains(&step.name("dist-rustc")));
        assert!(plan.contains(&step.name("dist-std")));
        assert!(plan.contains(&step.name("dist-src")));

        assert!(plan.contains(&step.target("B").name("dist-docs")));
        assert!(plan.contains(&step.target("B").name("dist-mingw")));
        assert!(!plan.contains(&step.target("B").name("dist-rustc")));
        assert!(plan.contains(&step.target("B").name("dist-std")));
        assert!(!plan.contains(&step.target("B").name("dist-src")));
    }

    #[test]
    fn dist_with_hosts() {
        let build = build(&["dist"], &["B"], &[]);
        let rules = super::build_rules(&build);
        let plan = rules.plan();
        println!("rules: {:#?}", plan);
        assert!(plan.iter().all(|s| s.stage == 2));

        let step = super::Step {
            name: "",
            stage: 2,
            host: &build.config.build,
            target: &build.config.build,
        };

        assert!(!plan.iter().any(|s| s.host == "B"));

        assert!(plan.contains(&step.name("dist-docs")));
        assert!(plan.contains(&step.name("dist-mingw")));
        assert!(plan.contains(&step.name("dist-rustc")));
        assert!(plan.contains(&step.name("dist-std")));
        assert!(plan.contains(&step.name("dist-src")));

        assert!(plan.contains(&step.target("B").name("dist-docs")));
        assert!(plan.contains(&step.target("B").name("dist-mingw")));
        assert!(plan.contains(&step.target("B").name("dist-rustc")));
        assert!(plan.contains(&step.target("B").name("dist-std")));
        assert!(!plan.contains(&step.target("B").name("dist-src")));
    }

    #[test]
    fn dist_with_targets_and_hosts() {
        let build = build(&["dist"], &["B"], &["C"]);
        let rules = super::build_rules(&build);
        let plan = rules.plan();
        println!("rules: {:#?}", plan);
        assert!(plan.iter().all(|s| s.stage == 2));

        let step = super::Step {
            name: "",
            stage: 2,
            host: &build.config.build,
            target: &build.config.build,
        };

        assert!(!plan.iter().any(|s| s.host == "B"));
        assert!(!plan.iter().any(|s| s.host == "C"));

        assert!(plan.contains(&step.name("dist-docs")));
        assert!(plan.contains(&step.name("dist-mingw")));
        assert!(plan.contains(&step.name("dist-rustc")));
        assert!(plan.contains(&step.name("dist-std")));
        assert!(plan.contains(&step.name("dist-src")));

        assert!(plan.contains(&step.target("B").name("dist-docs")));
        assert!(plan.contains(&step.target("B").name("dist-mingw")));
        assert!(plan.contains(&step.target("B").name("dist-rustc")));
        assert!(plan.contains(&step.target("B").name("dist-std")));
        assert!(!plan.contains(&step.target("B").name("dist-src")));

        assert!(plan.contains(&step.target("C").name("dist-docs")));
        assert!(plan.contains(&step.target("C").name("dist-mingw")));
        assert!(!plan.contains(&step.target("C").name("dist-rustc")));
        assert!(plan.contains(&step.target("C").name("dist-std")));
        assert!(!plan.contains(&step.target("C").name("dist-src")));
    }

    #[test]
    fn dist_target_with_target_flag() {
        let build = build(&["dist", "--target=C"], &["B"], &["C"]);
        let rules = super::build_rules(&build);
        let plan = rules.plan();
        println!("rules: {:#?}", plan);
        assert!(plan.iter().all(|s| s.stage == 2));

        let step = super::Step {
            name: "",
            stage: 2,
            host: &build.config.build,
            target: &build.config.build,
        };

        assert!(!plan.iter().any(|s| s.target == "A"));
        assert!(!plan.iter().any(|s| s.target == "B"));
        assert!(!plan.iter().any(|s| s.host == "B"));
        assert!(!plan.iter().any(|s| s.host == "C"));

        assert!(plan.contains(&step.target("C").name("dist-docs")));
        assert!(plan.contains(&step.target("C").name("dist-mingw")));
        assert!(!plan.contains(&step.target("C").name("dist-rustc")));
        assert!(plan.contains(&step.target("C").name("dist-std")));
        assert!(!plan.contains(&step.target("C").name("dist-src")));
    }

    #[test]
    fn dist_host_with_target_flag() {
        let build = build(&["dist", "--host=B", "--target=B"], &["B"], &["C"]);
        let rules = super::build_rules(&build);
        let plan = rules.plan();
        println!("rules: {:#?}", plan);
        assert!(plan.iter().all(|s| s.stage == 2));

        let step = super::Step {
            name: "",
            stage: 2,
            host: &build.config.build,
            target: &build.config.build,
        };

        assert!(!plan.iter().any(|s| s.target == "A"));
        assert!(!plan.iter().any(|s| s.target == "C"));
        assert!(!plan.iter().any(|s| s.host == "B"));
        assert!(!plan.iter().any(|s| s.host == "C"));

        assert!(plan.contains(&step.target("B").name("dist-docs")));
        assert!(plan.contains(&step.target("B").name("dist-mingw")));
        assert!(plan.contains(&step.target("B").name("dist-rustc")));
        assert!(plan.contains(&step.target("B").name("dist-std")));
        assert!(plan.contains(&step.target("B").name("dist-src")));

        let all = rules.expand(&plan);
        println!("all rules: {:#?}", all);
        assert!(!all.contains(&step.name("rustc")));
        assert!(!all.contains(&step.name("build-crate-test").stage(1)));

        // all stage0 compiles should be for the build target, A
        for step in all.iter().filter(|s| s.stage == 0) {
            if !step.name.contains("build-crate") {
                continue
            }
            println!("step: {:?}", step);
            assert!(step.host != "B");
            assert!(step.target != "B");
            assert!(step.host != "C");
            assert!(step.target != "C");
        }
    }

    #[test]
    fn build_default() {
        let build = build(&["build"], &["B"], &["C"]);
        let rules = super::build_rules(&build);
        let plan = rules.plan();
        println!("rules: {:#?}", plan);
        assert!(plan.iter().all(|s| s.stage == 2));

        let step = super::Step {
            name: "",
            stage: 2,
            host: &build.config.build,
            target: &build.config.build,
        };

        // rustc built for all for of (A, B) x (A, B)
        assert!(plan.contains(&step.name("librustc")));
        assert!(plan.contains(&step.target("B").name("librustc")));
        assert!(plan.contains(&step.host("B").target("A").name("librustc")));
        assert!(plan.contains(&step.host("B").target("B").name("librustc")));

        // rustc never built for C
        assert!(!plan.iter().any(|s| {
            s.name.contains("rustc") && (s.host == "C" || s.target == "C")
        }));

        // test built for everything
        assert!(plan.contains(&step.name("libtest")));
        assert!(plan.contains(&step.target("B").name("libtest")));
        assert!(plan.contains(&step.host("B").target("A").name("libtest")));
        assert!(plan.contains(&step.host("B").target("B").name("libtest")));
        assert!(plan.contains(&step.host("A").target("C").name("libtest")));
        assert!(plan.contains(&step.host("B").target("C").name("libtest")));

        let all = rules.expand(&plan);
        println!("all rules: {:#?}", all);
        assert!(all.contains(&step.name("rustc")));
        assert!(all.contains(&step.name("libstd")));
    }

    #[test]
    fn build_filtered() {
        let build = build(&["build", "--target=C"], &["B"], &["C"]);
        let rules = super::build_rules(&build);
        let plan = rules.plan();
        println!("rules: {:#?}", plan);
        assert!(plan.iter().all(|s| s.stage == 2));

        assert!(!plan.iter().any(|s| s.name.contains("rustc")));
        assert!(plan.iter().all(|s| {
            !s.name.contains("test") || s.target == "C"
        }));
    }

    #[test]
    fn test_default() {
        let build = build(&["test"], &[], &[]);
        let rules = super::build_rules(&build);
        let plan = rules.plan();
        println!("rules: {:#?}", plan);
        assert!(plan.iter().all(|s| s.stage == 2));
        assert!(plan.iter().all(|s| s.host == "A"));
        assert!(plan.iter().all(|s| s.target == "A"));

        assert!(plan.iter().any(|s| s.name.contains("-ui")));
        assert!(plan.iter().any(|s| s.name.contains("cfail")));
        assert!(plan.iter().any(|s| s.name.contains("cfail-full")));
        assert!(plan.iter().any(|s| s.name.contains("codegen-units")));
        assert!(plan.iter().any(|s| s.name.contains("debuginfo")));
        assert!(plan.iter().any(|s| s.name.contains("docs")));
        assert!(plan.iter().any(|s| s.name.contains("error-index")));
        assert!(plan.iter().any(|s| s.name.contains("incremental")));
        assert!(plan.iter().any(|s| s.name.contains("linkchecker")));
        assert!(plan.iter().any(|s| s.name.contains("mir-opt")));
        assert!(plan.iter().any(|s| s.name.contains("pfail")));
        assert!(plan.iter().any(|s| s.name.contains("rfail")));
        assert!(plan.iter().any(|s| s.name.contains("rfail-full")));
        assert!(plan.iter().any(|s| s.name.contains("rmake")));
        assert!(plan.iter().any(|s| s.name.contains("rpass")));
        assert!(plan.iter().any(|s| s.name.contains("rpass-full")));
        assert!(plan.iter().any(|s| s.name.contains("rustc-all")));
        assert!(plan.iter().any(|s| s.name.contains("rustdoc")));
        assert!(plan.iter().any(|s| s.name.contains("std-all")));
        assert!(plan.iter().any(|s| s.name.contains("test-all")));
        assert!(plan.iter().any(|s| s.name.contains("tidy")));
        assert!(plan.iter().any(|s| s.name.contains("valgrind")));
    }

    #[test]
    fn test_with_a_target() {
        let build = build(&["test", "--target=C"], &[], &["C"]);
        let rules = super::build_rules(&build);
        let plan = rules.plan();
        println!("rules: {:#?}", plan);
        assert!(plan.iter().all(|s| s.stage == 2));
        assert!(plan.iter().all(|s| s.host == "A"));
        assert!(plan.iter().all(|s| s.target == "C"));

        assert!(plan.iter().any(|s| s.name.contains("-ui")));
        assert!(!plan.iter().any(|s| s.name.contains("ui-full")));
        assert!(plan.iter().any(|s| s.name.contains("cfail")));
        assert!(!plan.iter().any(|s| s.name.contains("cfail-full")));
        assert!(plan.iter().any(|s| s.name.contains("codegen-units")));
        assert!(plan.iter().any(|s| s.name.contains("debuginfo")));
        assert!(!plan.iter().any(|s| s.name.contains("docs")));
        assert!(!plan.iter().any(|s| s.name.contains("error-index")));
        assert!(plan.iter().any(|s| s.name.contains("incremental")));
        assert!(!plan.iter().any(|s| s.name.contains("linkchecker")));
        assert!(plan.iter().any(|s| s.name.contains("mir-opt")));
        assert!(plan.iter().any(|s| s.name.contains("pfail")));
        assert!(plan.iter().any(|s| s.name.contains("rfail")));
        assert!(!plan.iter().any(|s| s.name.contains("rfail-full")));
        assert!(!plan.iter().any(|s| s.name.contains("rmake")));
        assert!(plan.iter().any(|s| s.name.contains("rpass")));
        assert!(!plan.iter().any(|s| s.name.contains("rpass-full")));
        assert!(!plan.iter().any(|s| s.name.contains("rustc-all")));
        assert!(!plan.iter().any(|s| s.name.contains("rustdoc")));
        assert!(plan.iter().any(|s| s.name.contains("std-all")));
        assert!(plan.iter().any(|s| s.name.contains("test-all")));
        assert!(!plan.iter().any(|s| s.name.contains("tidy")));
        assert!(plan.iter().any(|s| s.name.contains("valgrind")));
    }
}
