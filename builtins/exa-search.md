---
name: exa-search
description: Use Exa for semantic web research when EXA_API_KEY is available.
requires_env: EXA_API_KEY
---

# Exa Search

Use Exa when the user needs semantic web research: source discovery, high-quality
evidence, domain-filtered searches, or compact source extraction.

Docs: https://docs.exa.ai/reference/search

Recommended default: start with `POST https://api.exa.ai/search`, keep the query
natural-language, request a small result set, and retrieve highlights before
full text.

## Examples

Basic semantic search:

```bash
curl -sS https://api.exa.ai/search \
  -H "Content-Type: application/json" \
  -H "x-api-key: $EXA_API_KEY" \
  -d '{"query":"latest Exa search API docs","numResults":5}'
```

Compact evidence search. `contents.highlights` is the preferred first pass;
expand to full text only if highlights are not enough.

```bash
curl -sS https://api.exa.ai/search \
  -H "Content-Type: application/json" \
  -H "x-api-key: $EXA_API_KEY" \
  -d '{"query":"Brave Search API freshness parameter","numResults":5,"contents":{"highlights":true}}'
```

Domain-filtered variant. Use this non-default tweak when the user asks for
official docs or specific source families.

```bash
curl -sS https://api.exa.ai/search \
  -H "Content-Type: application/json" \
  -H "x-api-key: $EXA_API_KEY" \
  -d '{"query":"Exa search endpoint parameters","numResults":5,"includeDomains":["docs.exa.ai"],"contents":{"highlights":true}}'
```

## Useful Knobs

- `query`: required; natural-language queries work well.
- `numResults`: default service behavior is fine for broad search; set 3-10 to
  keep agent context compact.
- `type`: default service behavior is usually best; tweak only when comparing
  speed vs depth.
- `includeDomains` / `excludeDomains`: non-default; use for official docs,
  trusted-source searches, or source exclusion.
- Date filters: non-default; use when freshness is part of the request.
- `contents.highlights`: recommended compact retrieval.
- `contents.text`: non-default; use only when the answer needs full-page detail.

Return compact evidence: title, URL, date when available, short highlight, and
why the result matters. Cite the original source URLs, not Exa itself.
