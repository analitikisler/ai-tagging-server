# Contributing

Thanks for helping. Bug reports, fixes, new templates, docs and translations are all welcome. This page explains how the project is laid out, how to run it, and what a change needs before it's merged.

If you're planning something big, such as a new built-in template, a new admin page or a change to how hits are handled, open an issue first and describe it. That saves you from building something that doesn't fit.

## Getting started

You need a recent stable Rust (the crate uses edition 2024) and, for the checks on the console and the Cloudflare Worker, Node.js.

```sh
git clone https://github.com/analitikisler/ai-tagging-server.git
cd ai-tagging-server
cargo test
MEASUREMENT_IDS='*' ADMIN_TOKEN="$(openssl rand -hex 24)" cargo run
```

The server listens on port 8080. The admin console is at `http://localhost:8080/admin`; sign in with the token you set. To try a container file and the editing features (drafts, history, transformations), copy `examples/` somewhere and run with `CONTAINER_FILE=<copy>/container.json STATE_DIR=<a folder>`. Editing works on the copy, not on the file in the repository.

The AI features are optional. Set `ANTHROPIC_API_KEY` to try them; everything else works without it.

## How the code is laid out

| Where | What |
| --- | --- |
| `src/server.rs`, `src/request.rs` | The HTTP server: routing, the edge proxy's headers, the visitor IP. |
| `src/sandbox/` | The QuickJS sandbox that runs templates, and the sGTM-style APIs they `require()`. |
| `src/templates/` | Built-in templates: the GA4 client and tag, the Google scripts client, the ad platform tags. |
| `src/container.rs`, `src/container_file.rs`, `src/transform.rs` | Containers, firing rules, the container file editor and transformations. |
| `src/features.rs` | The GA4 client's optional features and how they are checked. |
| `src/admin.rs` | The admin API. |
| `src/ui/console.html` | The whole admin console: one HTML file with its own CSS and JavaScript, in English and Turkish. |
| `src/ai/` | The AI features. Each file is one task with its own prompt. |
| `src/checkup.rs`, `src/alerts.rs`, `src/replay.rs`, `src/history.rs`, `src/drafts.rs` | Setup check, alerts, replaying hits before a change, history and drafts. |
| `deploy/` | Cloud Run deploy script and the Cloudflare Worker. |
| `examples/` | A sample container with a custom template. |

## What a change needs

- **Tests.** Behavior changes come with a test. Template behavior is tested in `src/sandbox/tests.rs` by running real hits through a container; most modules have their own `tests` module.
- **The checks CI runs**, all passing:

  ```sh
  cargo fmt --check
  cargo clippy --all-targets --locked -- -D warnings
  cargo test --locked
  ```

- **Both languages in the console.** Every string in `src/ui/console.html` exists in the `en` and the `tr` blocks. If you don't speak Turkish, add the English text to both and say so in the pull request; someone will translate it.
- **Docs.** A new setting, environment variable, API route or template field is described in the README, where the others like it are.
- **No secrets or real data.** Examples and tests use `example.com`, `G-TEST000001` and other placeholders. Never commit tokens, keys, real measurement ids or real hits.

## How the project thinks

These come up in reviews, so here they are up front:

- **Hits never wait on the AI.** The AI works on the setup (explaining, auditing, writing rules, reviewing changes). Every hit is handled by the server alone.
- **New features are off by default** and change nothing for existing setups until someone turns them on.
- **Data changes are explicit.** The server doesn't rename, lowercase or "normalize" event data on its own; that's what transformations are for, and the user writes them.
- **Free and self-hosted.** A feature must work on someone's own server without a paid service. Integrations with paid services are fine when they're optional.
- **Nothing reaches a vendor without a tag.** Values the server adds for its own use go under `x-` keys, which the GA4 tag doesn't forward.
- **Templates stay sGTM-compatible.** Use the same `require()` API names and behavior as Google's sandboxed JavaScript, so templates can move between the two.

## Writing style

Comments and docs are plain, short sentences that say what the code does and why. Name things after what they are for, not how they work. Match the code around you.

## Commits and pull requests

- One topic per pull request. Small ones get reviewed faster.
- Write the commit subject as an imperative sentence ("Add click id restore to the GA4 client"), and use the body to say why.
- Fill in the pull request template: what changed, how you tested it, and anything a reviewer should look at closely.

## Reporting security issues

Please don't open a public issue for a vulnerability. See [SECURITY.md](SECURITY.md).

## License

By contributing, you agree that your contribution is licensed under the [MIT License](LICENSE), like the rest of the project.
