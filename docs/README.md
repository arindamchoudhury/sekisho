# Sekisho documentation

These pages describe what Sekisho does **today**, while release 1 is being
built. The [README](../README.md) describes the whole of release 1, much of
which doesn't exist yet: the web UI, the `sekisho` CLI for developers, and
delivering catalog entries to coding tools and calling MCP servers and A2A
agents through Sekisho (catalogs exist, and environments list their
entries). What these pages describe is built and tested, most of it end
to end against agentgateway v1.7.0 and Dex v2.46.1. Catalogs are proven
end to end against a Git server, a container registry and agent cards in
the mock, not against GitHub, GitLab, Docker Hub, GHCR or real agents; Git
mode's pull requests are proven against a mock of GitHub's API, not GitHub
itself.
The end-to-end tests send every request through agentgateway to a mock
provider that speaks each API (Anthropic Messages, Chat Completions,
Responses, Gemini, Bedrock Converse and Azure's paths), and check what
reached it and what was charged. **None of them calls a real provider
yet**, so nothing is proven against the real services: not Anthropic,
OpenAI or Gemini, not Bedrock (its AWS signing and cloud identity are
checked only as the mock receives them), not Azure (a key, an Entra
service principal, the cloud identity or Claude on Foundry), and not the
open-weight hosts or the host presets. Where a page says how a provider
answers, it comes from agentgateway's source and the providers' own
documentation.

| Page | What it covers |
|---|---|
| [Running Sekisho](running.md) | The `sekisho` binary's commands, its settings, and what it needs from PostgreSQL, Dex and agentgateway |
| [The API](api.md) | Who may call what, JSON conventions, error answers, and every route |
| [Providers and models](providers-and-models.md) | Model providers, their credentials, OpenAI-compatible hosts and presets, models and their prices |
| [Environments and keys](environments-and-keys.md) | Environments, who may use them, their catalog entries and toolkits, default models, labels, YAML, and environment keys |
| [Budgets and spend](budgets-and-spend.md) | Dollar budgets, and how each request is charged |
| [The gateway](gateway.md) | Calling models through an environment's routes, which client API reaches which model, and every refusal |
| [Catalogs](catalogs.md) | Catalog sources (MCP servers, A2A agents, skills, plugins, prompts), syncs, entries, owners, quarantine, registry credentials and importing Obot catalogs |
| [People and sign-in](people-and-sign-in.md) | Signing in, tokens, the first admin, local accounts, identity providers, groups, admins and offboarding |
| [Testing on Linux](testing-on-linux.md) | `make check`: building Sekisho and running every test, the images and the end-to-end suite on a Linux machine |
| [Testing on Linux by hand](testing-on-linux-by-hand.md) | The same checks as commands to run one at a time, with a results checklist |

Sekisho runs [agentgateway](https://github.com/agentgateway/agentgateway),
unmodified, in front of the model providers, and answers its checks. People
sign in through [Dex](https://github.com/dexidp/dex), which Sekisho manages.
