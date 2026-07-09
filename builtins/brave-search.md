---
name: brave-search
description: Use Brave for current web search.
requires_env: BRAVE_API_KEY
---

# Brave Search

Use Brave when the user needs broad current web lookup: recent facts, SERP-style
cross-checking, freshness filters, or search operators.

Docs:
- Web Search: https://api-dashboard.search.brave.com/api-reference/web/search/get
- LLM Context: https://api-dashboard.search.brave.com/documentation/services/llm-context

The `BRAVE_API_KEY` is available in your environment and can be directly used.

Note: Brave limits concurrency to 1. Search sequentially.

## Examples

Basic search using service defaults:

```bash
curl -sS --get "https://api.search.brave.com/res/v1/web/search" \
  -H "Accept: application/json" \
  -H "X-Subscription-Token: $BRAVE_API_KEY" \
  --data-urlencode "q=latest Brave Search API docs"
```

Use `freshness` when the user asks for recent results. Useful values
include `pd`, `pw`, `pm`, `py`.

```bash
curl -sS --get "https://api.search.brave.com/res/v1/web/search" \
  -H "Accept: application/json" \
  -H "X-Subscription-Token: $BRAVE_API_KEY" \
  --data-urlencode "q=OpenAI API changes" \
  --data-urlencode "freshness=pw"
```
