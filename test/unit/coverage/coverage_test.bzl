"""The collector runs with the test, which can have its own execution platform."""

load("@bazel_skylib//lib:unittest.bzl", "analysistest", "asserts")

_CollectorInfo = provider(
    "The execution platform selected for a coverage collector.",
    fields = {"uses_test_platform": "Whether the collector has the test platform constraint"},
)

def _collector_aspect_impl(_target, ctx):
    if hasattr(ctx.rule.attr, "_collect_cc_coverage"):
        return [ctx.rule.attr._collect_cc_coverage[_CollectorInfo]]
    return [_CollectorInfo(uses_test_platform = ctx.target_platform_has_constraint(ctx.attr._tester[platform_common.ConstraintValueInfo]))]

_collector_aspect = aspect(
    implementation = _collector_aspect_impl,
    attr_aspects = ["_collect_cc_coverage"],
    attrs = {"_tester": attr.label(default = ":tester")},
)

def _collector_platform_test_impl(ctx):
    env = analysistest.begin(ctx)
    target = analysistest.target_under_test(env)
    asserts.true(env, target[_CollectorInfo].uses_test_platform, "Coverage collector must use the test execution platform")
    return analysistest.end(env)

collector_platform_test = analysistest.make(
    _collector_platform_test_impl,
    config_settings = {
        "//command_line_option:extra_execution_platforms": [
            str(Label(":compiler_platform")),
            str(Label(":test_platform")),
        ],
        "//command_line_option:platforms": str(Label(":test_platform")),
    },
    extra_target_under_test_aspects = [_collector_aspect],
)
