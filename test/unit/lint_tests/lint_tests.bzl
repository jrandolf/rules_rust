"""Unit tests for `rust_clippy_test` and `rustfmt_test`.

One test per rule. Each verifies that with `transitive = True`
the collected marker count matches the dep graph, and that a crate tagged
for opt-out is skipped without breaking propagation to its own deps.
"""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")
load(
    "//rust:defs.bzl",
    "rust_binary",
    "rust_clippy_test",
    "rust_library",
    "rustfmt_test",
)

# Graph shared by both tests:
#
#   bin  -->  tagged_mid  -->  leaf
#
# `tagged_mid` carries `no_clippy` + `no_rustfmt`. With `transitive = True`
# both `bin` and `leaf` should get markers (2 total); `tagged_mid` is
# skipped but does NOT stop propagation to `leaf`.

def _markers(target):
    raw = target[RunEnvironmentInfo].environment.get("RUST_LINT_TEST_MARKERS", "")
    return [s for s in raw.replace(";", ":").split(":") if s]

def _make_transitive_count_test(marker_suffix, output_group_name):
    def _impl(ctx):
        env = analysistest.begin(ctx)
        tut = analysistest.target_under_test(env)
        markers = _markers(tut)

        bin_marker = "bin" + marker_suffix
        leaf_marker = "leaf" + marker_suffix
        tagged_marker = "tagged_mid" + marker_suffix

        asserts.equals(
            env,
            2,
            len([m for m in markers if m.endswith(marker_suffix)]),
            "Expected 2 '{}' markers, got {}".format(marker_suffix, markers),
        )
        asserts.true(
            env,
            any([bin_marker in m for m in markers]),
            "Expected top-level bin marker in {}".format(markers),
        )
        asserts.true(
            env,
            any([leaf_marker in m for m in markers]),
            "Expected leaf-through-tagged-crate marker in {}".format(markers),
        )
        for m in markers:
            asserts.false(
                env,
                tagged_marker in m,
                "Tagged crate should not have produced marker {}".format(m),
            )

        # The output group is what `bazel build --output_groups=<name>`
        # drives; without it, only the runner symlink is a default output
        # and the lint aspect never fires under a `build` invocation.
        og_files = getattr(tut[OutputGroupInfo], output_group_name).to_list()
        asserts.equals(
            env,
            sorted([m.split("/")[-1] for m in markers]),
            sorted([f.basename for f in og_files]),
            "`{}` output group must match RUST_LINT_TEST_MARKERS".format(output_group_name),
        )
        return analysistest.end(env)

    return analysistest.make(_impl)

clippy_transitive_test = _make_transitive_count_test(".clippy.ok", "clippy_checks")
rustfmt_transitive_test = _make_transitive_count_test(".rustfmt.ok", "rustfmt_checks")

_RunnerInfo = provider(fields = {"file": "Underlying lint runner executable"})

def _runner_aspect_impl(_target, ctx):
    return [_RunnerInfo(file = ctx.rule.attr._runner[DefaultInfo].files_to_run.executable)]

_runner_aspect = aspect(implementation = _runner_aspect_impl)

def _runner_format_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)
    runner = target[_RunnerInfo].file
    executable = target[DefaultInfo].files_to_run.executable

    asserts.equals(env, runner.extension, executable.extension)

    markers = target[RunEnvironmentInfo].environment["RUST_LINT_TEST_MARKERS"]
    separator = ";" if runner.extension == "exe" else ":"
    asserts.equals(env, 1, markers.count(separator))

    return analysistest.end(env)

_runner_format_test = analysistest.make(
    _runner_format_test_impl,
    extra_target_under_test_aspects = [_runner_aspect],
)

def _runner_platform_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)
    runner = target[_RunnerInfo].file
    asserts.true(
        env,
        runner.path.startswith("bazel-out/macos_x86_64-"),
        "Expected the test-platform runner, got {}".format(runner.path),
    )
    return analysistest.end(env)

_runner_platform_test = analysistest.make(
    _runner_platform_test_impl,
    config_settings = {
        "//command_line_option:extra_execution_platforms": [
            str(Label("//test/unit:macos_aarch64")),
            str(Label("//test/unit:macos_x86_64")),
        ],
        "//command_line_option:platforms": str(Label("//test/unit:macos_x86_64")),
    },
    extra_target_under_test_aspects = [_runner_aspect],
)

def lint_tests_suite(name):
    """Wire up the fixture graph and the two analysistests.

    Args:
        name: The name for the enclosing test_suite.
    """
    rust_library(
        name = "leaf",
        srcs = ["src/lib.rs"],
        crate_name = "leaf",
        crate_root = "src/lib.rs",
        edition = "2021",
    )
    rust_library(
        name = "tagged_mid",
        srcs = ["src/lib.rs"],
        crate_name = "tagged_mid",
        crate_root = "src/lib.rs",
        edition = "2021",
        tags = [
            "no_clippy",
            "no_rustfmt",
        ],
        deps = [":leaf"],
    )
    rust_binary(
        name = "bin",
        srcs = ["src/main.rs"],
        crate_root = "src/main.rs",
        edition = "2021",
        deps = [":tagged_mid"],
    )

    rust_clippy_test(
        name = "clippy_fixture",
        targets = [":bin"],
        tags = ["manual"],
        transitive = True,
    )
    rustfmt_test(
        name = "rustfmt_fixture",
        targets = [":bin"],
        tags = ["manual"],
        transitive = True,
    )

    clippy_transitive_test(
        name = "clippy_transitive_test",
        target_under_test = ":clippy_fixture",
    )
    rustfmt_transitive_test(
        name = "rustfmt_transitive_test",
        target_under_test = ":rustfmt_fixture",
    )

    _runner_format_test(
        name = "clippy_runner_format_test",
        target_under_test = ":clippy_fixture",
    )
    _runner_format_test(
        name = "rustfmt_runner_format_test",
        target_under_test = ":rustfmt_fixture",
    )

    _runner_platform_test(
        name = "clippy_runner_platform_test",
        target_under_test = ":clippy_fixture",
        target_compatible_with = [
            "@platforms//cpu:aarch64",
            "@platforms//os:macos",
        ],
    )
    _runner_platform_test(
        name = "rustfmt_runner_platform_test",
        target_under_test = ":rustfmt_fixture",
        target_compatible_with = [
            "@platforms//cpu:aarch64",
            "@platforms//os:macos",
        ],
    )

    native.test_suite(
        name = name,
        tests = [
            ":clippy_transitive_test",
            ":rustfmt_transitive_test",
            ":clippy_runner_format_test",
            ":rustfmt_runner_format_test",
            ":clippy_runner_platform_test",
            ":rustfmt_runner_platform_test",
        ],
    )
