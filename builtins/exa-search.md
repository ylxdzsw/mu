---
name: exa-search
description: Use Exa for semantic web research.
requires_env: EXA_API_KEY
---

# Exa Search

Use Exa when the user needs semantic web research: source discovery, high-quality
evidence, domain-filtered searches, or compact source extraction.

Docs: https://docs.exa.ai/reference/search

The `EXA_API_KEY` is available in your environment and can be directly used.

## Examples

Basic search using service defaults:

```bash
curl -sS https://api.exa.ai/search \
  -H "Content-Type: application/json" \
  -H "x-api-key: $EXA_API_KEY" \
  -d '{"query":"latest Exa search API docs"}'
```

Compact evidence search for probing existence without needing details.

```bash
curl -sS https://api.exa.ai/search \
  -H "Content-Type: application/json" \
  -H "x-api-key: $EXA_API_KEY" \
  -d '{"query":"Brave Search API freshness parameter","numResults":5,"contents":{"highlights":true}}'
```

Domain-filtered variant. Use this for official docs or specific source families.

```bash
curl -sS https://api.exa.ai/search \
  -H "Content-Type: application/json" \
  -H "x-api-key: $EXA_API_KEY" \
  -d '{"query":"Exa search endpoint parameters","numResults":5,"includeDomains":["docs.exa.ai"],"contents":{"highlights":true}}'
```
