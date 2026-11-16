# Providers and models

A **provider** is where models run, with the credential Sekisho uses there.
A **model** is one model of one provider, with its prices. Environments
then list models from any providers ([Environments and keys](environments-and-keys.md)).
Only admins manage providers and models.

Credentials are kept encrypted in PostgreSQL (`SEKISHO_ENCRYPTION_KEY`), go
only to agentgateway's config, and are never answered back by the API or
written to the log. People and their coding tools never see them: they use
their own session tokens or environment keys.

## Providers

| `kind` | What | Credentials | Settings |
|---|---|---|---|
| `anthropic` | Anthropic's API | `api_key` | `base_url` optional |
| `openai` | OpenAI's API | `api_key` | `base_url` optional |
| `gemini` | Google's Gemini API | `api_key`: a Gemini API key (`AIza…` or `AQ.…`), which agentgateway sends to Gemini's own endpoints in `x-goog-api-key`; a value of any other shape goes as a bearer token | `base_url` optional |
| `bedrock` | Amazon Bedrock (through its Converse API) | `api_key` (a Bedrock API key), AWS access keys, or `cloud_identity` | `region` required, such as `us-east-1`; `base_url` optional |
| `azure` | Azure OpenAI, or Microsoft Foundry | `api_key`, an Entra service principal, or `cloud_identity` | `endpoint` required |
| `openai_compatible` | Any other host that speaks OpenAI's API, such as Scaleway, Groq or a vLLM or Ollama server | `api_key` | `base_url` and `apis` required; `catalog_key` optional |

A provider takes exactly one credential, in one of these shapes:

| Shape | Fields |
|---|---|
| API key | `api_key` |
| AWS access keys | `aws_access_key_id` and `aws_secret_access_key`, with an optional `aws_session_token` |
| Entra service principal | `azure_tenant_id` (the tenant's ID or a domain such as `contoso.onmicrosoft.com`), `azure_client_id` and `azure_client_secret` |
| The gateway's own cloud identity | `"cloud_identity": true`: agentgateway uses the AWS or Azure identity it finds where it runs ([below](#the-gateways-cloud-identity)); Sekisho stores no secret |

Names are 1–63 lowercase letters, digits or hyphens, starting and ending
with a letter or digit, and unique.

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/providers \
  -d '{"name": "anthropic", "kind": "anthropic", "api_key": "sk-ant-..."}'
```

The answer, like every provider answer, shows the credential's shape and
never its value: `"credential": "key"` (or `aws_access_keys`,
`azure_client_secret`, `cloud_identity`).

### Base URLs

A `base_url` sends a provider's requests somewhere other than its usual
host, such as a proxy or a regional endpoint. For an OpenAI-compatible host
it is required, written as the host documents it, such as
`https://api.scaleway.ai/v1`.

- It must be `http://` or `https://` with a host, and no user name,
  password, query or fragment.
- Sekisho reads it exactly as agentgateway will, and refuses one
  agentgateway couldn't read: an invalid IP address (`10.0.0.256`), host
  or port, a backslash, or a space. agentgateway would otherwise refuse
  its whole config file, and no environment's changes would reach it. A
  provider saved by an earlier version with such a URL is left out of the
  gateway's config, with its models, and Sekisho logs a warning naming it,
  until its `base_url` is fixed (`PATCH`, with its credential).
- Sekisho resolves the host when the provider is saved, as agentgateway
  reads it: lowercased, percent-decoded, and an IP address in any form
  (`0xa9.254.169.254` is `169.254.169.254`). Loopback and private network
  addresses, and hosts that don't resolve from Sekisho (they may resolve
  on the gateway's network), need `"allow_private_network": true`.
  Link-local and cloud metadata addresses are never allowed.
- With AWS access keys or the cloud identity, a Bedrock provider's
  `base_url` must be an `https://` host ending in `.amazonaws.com` or
  `.api.aws`, and with an Entra service principal or the cloud identity an
  Azure provider's must end in `.openai.azure.com`,
  `.services.ai.azure.com` or `.cognitiveservices.azure.com`, so a cloud
  credential never goes anywhere else.

### Amazon Bedrock

```json
{"name": "bedrock", "kind": "bedrock", "region": "eu-west-1",
 "aws_access_key_id": "AKIA...", "aws_secret_access_key": "..."}
```

Every Bedrock model the account can call works, Claude included, through
the Anthropic Messages, Chat Completions and Responses APIs
([which API reaches which model](gateway.md#which-api-reaches-which-model)).
A Bedrock model's name is its Bedrock model ID, such as
`anthropic.claude-sonnet-4-5-20250929-v1:0`, or an inference profile ID,
such as `us.anthropic.claude-sonnet-4-5-20250929-v1:0`, which many newer
models need. Claude is recognised by `anthropic.claude` anywhere in the
name. A Claude model Bedrock serves only through its newer Mantle
endpoint (`anthropic.claude-mythos-5` in agentgateway v1.7.0's catalog)
takes only the Messages API, so Codex can't use it ([which API reaches
which model](gateway.md#which-api-reaches-which-model)).

### Azure OpenAI and Microsoft Foundry

`endpoint` is the endpoint the Azure portal shows, in one of these forms:

- `https://<resource>.openai.azure.com` (Azure OpenAI), optionally ending
  in `/` or `/openai/v1`;
- `https://<resource>.services.ai.azure.com` (Foundry);
- `https://<resource>.services.ai.azure.com/api/projects/<project>` (a
  Foundry project).

An Azure model's name is its **deployment name**, the name you gave the
deployment in Azure, not the model's own name (`gpt-5-prod`, say, for a
deployment of `gpt-5`). A deployment name that starts with `claude` (in
any case) is Claude, which Azure serves only on Foundry and only with
Entra sign-in, so name Claude deployments that way. So such a model needs a
Foundry endpoint and an Entra service principal or the cloud identity; an
Azure OpenAI endpoint or an API key is refused when the model is added, or
when the provider's endpoint or credential changes later.

### The gateway's cloud identity

With `"cloud_identity": true`, Sekisho stores no secret: agentgateway signs
in as itself, so give it the identity where it runs.

- **Bedrock:** the AWS SDK's usual credential chain: `AWS_*` environment
  variables, a profile, an EKS service account's role (IRSA or Pod
  Identity), or an ECS task or EC2 instance role. The role needs Bedrock's
  invoke permissions for the models.
- **Azure:** workload identity on Kubernetes, or a managed identity on an
  Azure VM; on a developer's machine, developer tools such as the Azure
  CLI's sign-in. The identity needs a role that may call the models, such
  as *Cognitive Services User*, or *Foundry User* on Foundry. An Entra
  service principal needs the same role.

Every provider with the cloud identity uses the same one: agentgateway's.

### OpenAI-compatible hosts

An OpenAI-compatible provider says which APIs its host serves, in `apis`:

- `chat_completions`, always;
- `messages` (Anthropic's Messages API) and `responses` (OpenAI's
  Responses API), where the host serves them.

Each may have a `path`, the whole upstream path when the host doesn't
serve that API at the usual place under the base URL. A path is `/`
followed by up to 255 of `A-Z a-z 0-9 . _ ~ / -`, with no empty, `.` or
`..` segment. agentgateway passes a request through in an API the host
serves, and translates it otherwise, so every coding tool can use the
host's models.

```json
{"name": "deepseek", "kind": "openai_compatible", "base_url": "https://api.deepseek.com/v1",
 "apis": [{"api": "chat_completions"},
          {"api": "messages", "path": "/anthropic/v1/messages"},
          {"api": "responses"}],
 "catalog_key": "deepseek", "api_key": "..."}
```

Without `apis` (and without a preset), the host serves Chat Completions
only.

`catalog_key` is the name agentgateway's model catalog files the host's
prices under, such as `groq`: up to 64 lowercase letters, digits, dots,
underscores or hyphens, starting with a letter or digit. With it, a model
the catalog knows needs no prices of its own ([Models](#models)).

### Host presets

A `preset` fills in a known host's kind, base URL, APIs and catalog key;
anything the request gives itself wins, and every field can be changed
later. `GET /api/v1/provider-presets` lists them:

| `preset` | Host | Base URL | APIs | Catalog key |
|---|---|---|---|---|
| `baseten` | Baseten | `https://inference.baseten.co/v1` | Chat Completions, Messages | `baseten` |
| `cerebras` | Cerebras | `https://api.cerebras.ai/v1` | Chat Completions | `cerebras` |
| `deepinfra` | DeepInfra | `https://api.deepinfra.com/v1/openai` | Chat Completions, Messages (`/anthropic/v1/messages`) | `deepinfra` |
| `deepseek` | DeepSeek | `https://api.deepseek.com/v1` | Chat Completions, Messages (`/anthropic/v1/messages`), Responses | `deepseek` |
| `fireworks` | Fireworks | `https://api.fireworks.ai/inference/v1` | Chat Completions, Messages, Responses | `fireworks` |
| `groq` | Groq | `https://api.groq.com/openai/v1` | Chat Completions, Responses | `groq` |
| `huggingface` | Hugging Face | `https://router.huggingface.co/v1` | Chat Completions, Responses | `huggingface` |
| `mistral` | Mistral | `https://api.mistral.ai/v1` | Chat Completions | `mistral` |
| `ollama` | Ollama | none: give your server's URL | Chat Completions, Responses | `ollama` |
| `openrouter` | OpenRouter | `https://openrouter.ai/api/v1` | Chat Completions, Messages, Responses | `openrouter` |
| `scaleway` | Scaleway | `https://api.scaleway.ai/v1` | Chat Completions | none |
| `togetherai` | Together | `https://api.together.xyz/v1` | Chat Completions | `togetherai` |

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/providers \
  -d '{"name": "scaleway", "preset": "scaleway", "api_key": "..."}'
```

### Changing a provider

`PATCH /api/v1/providers/{name}` changes only the fields it is given.
`"base_url": null` removes a base URL (not an OpenAI-compatible host's),
and `"catalog_key": null` removes a catalog key, unless a model would then
have no prices. The name and kind never change.

Changing where requests go (`base_url`, `region` or `endpoint`) needs the
credential again in the same request, so a stored secret is never sent to a
new place without someone who knows it. Changing `apis` or `catalog_key`
doesn't. Giving a credential alone replaces the stored one.

`DELETE /api/v1/providers/{name}` answers 409 `in_use`, naming the models,
while the provider has any.

## Models

A model's name is the ID its provider knows it by, such as
`claude-sonnet-4-5` or `anthropic.claude-sonnet-4-5-20250929-v1:0`: 1–128
letters, digits or `. _ : / -`, starting with a letter or digit. Names are
unique across Sekisho, so one model ID is served by one provider.

```bash
curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  https://sekisho.example.com/api/v1/models \
  -d '{"name": "mistral-small-3.2", "provider": "scaleway", "input_price": "0.15", "output_price": "0.35"}'
```

Prices are dollars per 1,000,000 tokens:

- `input_price` and `output_price` go together. Leave both out to use the
  price in agentgateway's model catalog.
- `cache_read_price` and `cache_write_price` are optional and need input
  and output prices. Without them, Sekisho uses the provider's usual ratio
  to the input price: for Anthropic models and Claude on Bedrock or Azure,
  0.1 to read and 1.25 to write; for OpenAI, other Bedrock and Azure
  models, 0.5 and 1; for Gemini, 0.25 and 1; for OpenAI-compatible hosts,
  the input price for both.
- An OpenAI-compatible host's model needs prices unless its provider has a
  catalog key under which agentgateway's catalog has the model.

`DELETE /api/v1/models/{name}` answers 409 `in_use`, naming the
environments, while any environment lists the model.

A model of a built-in provider that has no prices of its own, and that
agentgateway's catalog doesn't know, is charged nothing. Give such a model
prices.
