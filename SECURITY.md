# Security policy

## Supported versions
Only the latest release (and `main`) receives security fixes. The project is pre-1.0.

## Threat model
claude-manager is a local tool for one user on one machine.
- It listens on `127.0.0.1` only and must never be exposed on `0.0.0.0` or through a tunnel.
- Requests with a wrong `Host` or `Origin` header are rejected (DNS rebinding, cross-site requests).
- A random token is generated on every run. The page is served only to a client that logged in via the one-time
  `?k=<token>` link, which sets an HttpOnly, SameSite=Strict cookie. Writes, terminals and WebSockets require it.
- Terminal panes run the real `claude` CLI as the current user, so anyone holding the token can run commands as that
  user. Treat the token and the start URL like a password.
- Claude's own `.jsonl` files are not rewritten (except an appended `custom-title` line when an idle session is
  renamed); child processes are started from argv, never through a shell.

In scope: auth or Host/Origin bypass, token leakage, path traversal, command injection, anything reachable from another
website or another local user. Out of scope: attacks that already need code execution as the same user.

## Reporting a vulnerability
Please do not open a public issue. Use GitHub "Report a vulnerability" on the Security tab
(https://github.com/khoahotran/claude-manager/security/advisories/new). Include steps to reproduce and the affected version.
Expect an acknowledgement within a few days; fixes are released before details are published.
