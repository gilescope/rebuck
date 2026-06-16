// bazel-remote release artifacts + their SHA-256 digests, keyed by GHA-runner
// platform-arch. Pinned per version: bumping `bazel-remote-version` past a
// version listed here requires adding its checksums (the action fails closed on
// an unknown version rather than skipping verification).
export const CHECKSUMS: Record<string, Record<string, string>> = {
  "2.6.1": {
    "linux-amd64": "025d53aeb03a7fdd4a0e76262a5ae9eeee9f64d53ca510deff1c84cf3f276784",
    "linux-arm64": "b8b9456d669d45bb8c5480ce0529ca4fa9d445e0c33b3aeed779df802d8164db",
    "darwin-amd64": "02140dd308ca3f175ac198bf57a8b60c65d047d8957fa9edbe09e3d549735392",
    "darwin-arm64": "45a28a3b7e4466b5340577fc5618088e188e5ef306e02f0212108edee312bb1b",
  },
};

// Maps Node's process.platform/arch to bazel-remote's asset naming.
export function platformArch(): string {
  const os = process.platform === "darwin" ? "darwin" : process.platform === "linux" ? "linux" : "";
  const arch = process.arch === "x64" ? "amd64" : process.arch === "arm64" ? "arm64" : "";
  if (!os || !arch) {
    throw new Error(`rebuck: unsupported runner ${process.platform}/${process.arch}`);
  }
  return `${os}-${arch}`;
}

export function downloadUrl(version: string, pa: string): string {
  return `https://github.com/buchgr/bazel-remote/releases/download/v${version}/bazel-remote-${version}-${pa}`;
}

export function checksum(version: string, pa: string): string {
  const v = CHECKSUMS[version];
  if (!v) {
    throw new Error(
      `rebuck: no pinned checksums for bazel-remote ${version}. Add them to src/constants.ts (fail-closed by design).`,
    );
  }
  const sum = v[pa];
  if (!sum) {
    throw new Error(`rebuck: no checksum for bazel-remote ${version} on ${pa}.`);
  }
  return sum;
}
