# Copy this into your repo as platforms/defs.bzl (alongside platforms/BUCK).
#
# Execution platform that keeps LOCAL execution but turns on the remote ACTION
# CACHE (read+write). buck2 then queries the cache by action digest; on a miss
# it runs the action locally and uploads the result. The rebuck action points
# [buck2_re_client] at a cache-only bazel-remote sidecar.
#
# The prelude's own execution_platform hardcodes remote_enabled=False (-> no
# cache). We need remote_enabled=True to activate the ActionCacheChecker path,
# remote_cache_enabled=True for read+write, and remote execution stays unused
# (bazel-remote advertises no Execution service, so misses fall back to local).

def _re_cache_execution_platform_impl(ctx: AnalysisContext) -> list[Provider]:
    constraints = dict()
    constraints.update(ctx.attrs.cpu_configuration[ConfigurationInfo].constraints)
    constraints.update(ctx.attrs.os_configuration[ConfigurationInfo].constraints)
    cfg = ConfigurationInfo(constraints = constraints, values = {})

    name = ctx.label.raw_target()
    platform = ExecutionPlatformInfo(
        label = name,
        configuration = cfg,
        executor_config = CommandExecutorConfig(
            local_enabled = True,
            remote_enabled = True,
            remote_cache_enabled = True,
            remote_execution_use_case = "buck2-default",
            remote_execution_properties = {},
            use_windows_path_separators = ctx.attrs.use_windows_path_separators,
        ),
    )

    return [
        DefaultInfo(),
        platform,
        PlatformInfo(label = str(name), configuration = cfg),
        ExecutionPlatformRegistrationInfo(platforms = [platform]),
    ]

re_cache_execution_platform = rule(
    impl = _re_cache_execution_platform_impl,
    attrs = {
        "cpu_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "os_configuration": attrs.dep(providers = [ConfigurationInfo]),
        "use_windows_path_separators": attrs.bool(default = False),
    },
)
