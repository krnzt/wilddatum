# Releasing WildDatum

WildDatum releases are built from a version tag. GitHub-hosted runners compile
Linux x86-64, macOS arm64, and macOS x86-64 executables. The two macOS binaries
are joined with `lipo` into one universal executable.

The release workflow produces, for each supported platform:

- a self-contained `.tar.gz` containing `bin/wilddatum` and the Rerun web bundle;
- a platform-narrowed `.mcpb` package;
- SHA-256 sidecars for both artifacts.

It then generates `server.json` with the immutable GitHub Release URLs and
actual MCPB hashes, creates a GitHub prerelease, validates the document with the
official `mcp-publisher`, authenticates using GitHub Actions OIDC, and publishes
`io.github.krnzt/wilddatum` to the official MCP Registry. No long-lived registry
credential is stored in the repository.

## Cut an alpha

1. Update the workspace, web, installer, and MCPB versions together.
2. Run the local validation commands from the README, including the browser E2E.
3. Commit and push `main`; wait for CI to pass.
4. Create and push the matching annotated tag, for example:

   ```bash
   git tag -a v0.1.0-alpha.1 -m "WildDatum v0.1.0-alpha.1"
   git push origin v0.1.0-alpha.1
   ```

5. Verify the release assets on a clean supported machine and confirm the
   registry response for `io.github.krnzt/wilddatum`.

macOS alpha binaries are intentionally unsigned and not notarized. Do not reuse
an unrelated Apple signing identity. Add project-owned Developer ID signing and
notarization only when the project has the appropriate credentials and release
policy.
