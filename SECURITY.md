# Security policy

Report vulnerabilities privately to the maintainers through GitHub Security
Advisories. Do not open a public issue containing credentials, private file
paths, or a working exploit.

WildDatum's security model assumes the local user trusts the installed native
binary. MCP access does not grant arbitrary filesystem reads: local sources are
registered out of band and exposed to agents only through opaque identifiers.

NEON credentials are stored in the operating-system keychain. For headless
environments, `NEON_API_TOKEN` is supported but should be injected by the host's
secret mechanism rather than stored in project configuration.

Community provider subprocesses are explicitly installed trusted local code;
they are not sandboxed plugins. WildDatum clears the child environment, sends no
credential values, bounds calls, and validates planned HTTPS origins, but an
installed executable retains the user's operating-system permissions. Review
its source and distribution before installation.

The browser explorer binds only to loopback and requires an unguessable launch
token. Treat the token as short-lived local authorization and do not publish its
URL. Viewer-reported point positions are not accepted as exact source values;
exact source-row queries require a server-verified instance mapping.
