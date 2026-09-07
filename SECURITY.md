# Security Policy

## Reporting a vulnerability

Please **do not** open a public issue for security vulnerabilities.

Instead, use GitHub's private vulnerability reporting: go to the
[Security tab](https://github.com/getgat-dev/gat/security) of this
repository and select **Report a vulnerability**. This opens a private
advisory visible only to you and the maintainers, where you can share
details and coordinate a fix and disclosure timeline.

## Scope

`gat` reads git repository data and talks to storage remotes (S3, Azure
Blob, or a local directory) using credentials from standard environment
variables. Vulnerabilities of particular interest include:

- Path traversal or symlink handling issues when tracking, checking out,
  or importing files
- Credential handling or leakage (remote URLs, access keys)
- Manifest (`.gat`) parsing issues that could lead to writing outside the
  intended working tree
