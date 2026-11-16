# Sekisho documentation

These pages describe what Sekisho does **today**, while release 1 is being
built. The [README](../README.md) describes the whole of release 1, much of
which doesn't exist yet: the web UI, the `sekisho` CLI for developers, MCP
servers, A2A agents and catalogs. What these pages describe is built and
tested, most of it end to end against agentgateway v1.7.0 and Dex v2.46.1.
The end-to-end tests send every request through agentgateway to a mock
provider that speaks each API (Anthropic Messages, Chat Completions,
Responses, Gemini, Bedrock Converse and Azure's paths), and check what
reached it and what was charged. Not yet proven against the real services:

- the open-weight hosts and the host presets;
- Azure with an Entra service principal or the cloud identity, and Claude
  on Foundry, with real accounts.

| Page | What it covers |
|---|---|
| [Running Sekisho](running.md) | The `sekisho` binary's commands, its settings, and what it needs from PostgreSQL, Dex and agentgateway |
| [The API](api.md) | Who may call what, JSON conventions, error answers, and every route |
| [Providers and models](providers-and-models.md) | Model providers, their credentials, OpenAI-compatible hosts and presets, models and their prices |
| [Environments and keys](environments-and-keys.md) | Environments, who may use them, and environment keys |
| [Budgets and spend](budgets-and-spend.md) | Dollar budgets, and how each request is charged |
| [The gateway](gateway.md) | Calling models through an environment's routes, which client API reaches which model, and every refusal |
| [People and sign-in](people-and-sign-in.md) | Signing in, tokens, the first admin, local accounts, identity providers, groups, admins and offboarding |

Sekisho runs [agentgateway](https://github.com/agentgateway/agentgateway),
unmodified, in front of the model providers, and answers its checks. People
sign in through [Dex](https://github.com/dexidp/dex), which Sekisho manages.
