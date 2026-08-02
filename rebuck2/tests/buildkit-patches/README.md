# buildkit patches

Applied on top of `EarthBuild/buildkit` at the `BUILDKIT_REF` pinned in
`.github/workflows/consolidation-earthbuild-ci.yml` -- the pushed tip of
`giles-rebuck-single-flight`. Anything not yet pushed to that branch lives here
instead, so CI can measure it without waiting on a push to an org repo.

`0001` is the one the coordinated arm cannot run without. `VertexOptions` are
deliberately excluded from the vertex digest (buildkit `solver/types.go:49`), so
`RUN --no-cache` and the same RUN without it produce the same lease key -- and a
follower adopts a result for a step whose contract is "execute me again".
Measured on `tests+no-cache-local-artifact-test`, which writes `/dev/urandom` to
a file twice under `--no-cache` and asserts the two differ:

| arm | result |
| ----------------------------------- | --------- |
| consolidated, `CONTROL=1` (no SF) | PASS 118s |
| consolidated, single-flight, before | FAIL 130s |
| consolidated, single-flight, after | PASS 134s |

To refresh after further work on the box:

```sh
git -C ~/git/EarthBuild/buildkit format-patch -o /tmp/bkp --no-signature \
    "$(git ls-remote git@github.com:EarthBuild/buildkit.git \
        giles-rebuck-single-flight | cut -f1)..HEAD"
```

Empty output means everything local is pushed and `BUILDKIT_REF` can simply move
forward instead.
