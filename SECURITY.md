# Security policy

## Supported versions

Only the latest release (the newest `vX.Y.Z` tag on `main`) receives security fixes. `develop` is
the integration branch, not a support channel.

## Reporting a vulnerability

Report privately through GitHub Private Vulnerability Reporting:
<https://github.com/PixmaNts/withnav/security/advisories/new>

Please do **not** open a public issue, pull request or discussion for a vulnerability.

Include as much of this as you can:

- the affected version and your operating system;
- how you installed WithNav (VSIX, `nav` binary, built from source, Nix);
- a description of the problem;
- steps to reproduce it;
- the impact, if you know it.

## What to expect

- Acknowledgement within 72 hours.
- Triage (confirmed or not, severity, plan) within 7 days.
- Coordinated disclosure: we agree on a date with you, fix first, then publish an advisory. You are
  credited in the advisory unless you prefer to stay anonymous.

## Scope

In scope:

- the Rust core (`nav`, `nav lsp`);
- the VS Code extension;
- the web client (`webui/`);
- the website and documentation;
- CI and release infrastructure.

Out of scope:

- vulnerabilities in third-party models or providers reached with your own API keys;
- issues that require physical access to your machine;
- theoretical issues with no concrete exploit path in a local-first tool.

## Threat model

WithNav is a local, local-first tool for one person at a keyboard. Its realistic risks are
malicious content in files the agent reads, prompt injection, and supply-chain issues. The
permission gate asks before `write`, `edit`, `bash` and `fetch` and guards against drift; it is
**not a sandbox**, and `bash` is not confined to the workspace.

Facts that matter when you assess impact:

- `nav serve` binds to loopback (`127.0.0.1`) by default. A non-loopback bind needs the explicit
  `--allow-non-loopback` flag.
- `nav tui` and `nav web` start a private server on loopback with a per-process bearer token.
- WithNav has no telemetry. It only contacts the model providers you configure, plus any URL the
  agent fetches with your approval.
