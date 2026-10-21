/**
 * Whether this environment must have a built aft binary for the e2e suites.
 *
 * CI always builds the binary before the Bun suites run, so a missing binary
 * there means setup broke, and the suites must fail instead of skipping into a
 * green run. CI providers spell the flag differently (`CI=true`, `CI=1`), and
 * GitHub Actions also sets `GITHUB_ACTIONS=true`. `AFT_REQUIRE_E2E_BINARY=1`
 * asks for the same strictness outside CI.
 */
export function e2eBinaryRequired(env: Record<string, string | undefined>): boolean {
  const ci = env.CI?.trim().toLowerCase();
  return (
    ci === "true" ||
    ci === "1" ||
    env.GITHUB_ACTIONS === "true" ||
    env.AFT_REQUIRE_E2E_BINARY === "1"
  );
}
