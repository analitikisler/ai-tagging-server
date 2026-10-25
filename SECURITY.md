# Security

ai tagging server runs third-party template code, handles visitors' personal data and sends requests on their behalf, so security reports matter a great deal. Thank you for taking the time.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: on the repository's **Security** tab, choose **Report a vulnerability**. Don't open a public issue, pull request or discussion for it.

Include what you can of:

- what the issue is and what an attacker could do with it,
- the version or commit you tested,
- steps or a request that shows it (a minimal container file or `curl` command is ideal),
- any configuration it depends on, such as `PROXY_SECRET`, `TENANTS_FILE` or a custom template.

Analitik İşler aims to acknowledge a report within 3 working days and to agree on a fix and a disclosure date with you. Reporters are credited in the release notes unless they'd rather not be named.

## Supported versions

The project is in early development. Fixes go into the `main` branch and the next release; older releases don't get backports yet.

## What's in scope

Some examples of what counts as a vulnerability:

- A template escaping the sandbox: reaching the host, the file system, environment variables or another template's storage, or getting past its declared permissions.
- The server sending requests to private, loopback, link-local or cloud metadata addresses, or following redirects there, without `OUTGOING_ALLOW_PRIVATE=1`.
- Using the server as an open relay: getting hits forwarded for a GA4 property, host or tenant it isn't configured for.
- Reaching the admin API or console without the admin token, or doing more with the check token than running the alert checks.
- Secrets (tokens, API keys, client secrets) showing up in the admin API, the console, traces, logs, history or an AI prompt.
- Plain personal data reaching a trace, the `__debug` report or a log where the README says it's hashed or dropped.
- Getting past `PROXY_SECRET`, or making the server trust the edge proxy's headers when the secret is set and wasn't sent.

Not in scope:

- Problems that need someone who already holds the admin token, unless they reach beyond what the token is meant to allow.
- What a template the operator chose to install does with the permissions it was given.
- Denial of service by volume. Put the server behind an edge proxy with rate limits, as the README describes.
- Findings from automated scanners without a demonstrated impact.

## Running it safely

The README's deploy section describes the recommended setup. In short: keep the server behind your edge proxy with `PROXY_SECRET` set, keep `ADMIN_TOKEN`, `CHECK_TOKEN`, `DEBUG_TOKEN` and API keys in a secret manager, list the GA4 ids you accept in `MEASUREMENT_IDS` (or per tenant), and give templates from others only the permissions they need.
