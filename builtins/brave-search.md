---
name: brave-search
description: Use Brave for current web search when BRAVE_API_KEY is available.
requires_env: BRAVE_API_KEY
---

# Brave Search

Use Brave when the user needs broad current web lookup: recent facts, SERP-style
cross-checking, freshness filters, local/current topics, or search operators.

Docs:
- Web Search: https://api-dashboard.search.brave.com/api-reference/web/search/get
- LLM Context variant: https://api-dashboard.search.brave.com/documentation/services/llm-context

Recommended default: start with Web Search,
`GET https://api.search.brave.com/res/v1/web/search`, and let Brave defaults
handle country, language, and mixed result selection unless the user asks
otherwise.

Use `curl --get --data-urlencode "q=..."` for queries. It avoids brittle manual
URL escaping for punctuation, quotes, operators, and non-ASCII text.

## Examples

Basic search using service defaults:

```bash
curl -sS --get "https://api.search.brave.com/res/v1/web/search" \
  -H "Accept: application/json" \
  -H "X-Subscription-Token: $BRAVE_API_KEY" \
  --data-urlencode "q=latest Brave Search API docs"
```

Freshness variant. `freshness` is non-default; use it when the user asks for
recent results.

```bash
curl -sS --get "https://api.search.brave.com/res/v1/web/search" \
  -H "Accept: application/json" \
  -H "X-Subscription-Token: $BRAVE_API_KEY" \
  --data-urlencode "q=OpenAI API changes" \
  --data-urlencode "freshness=pw"
```

Smaller web-only result set. `count` and `result_filter` are non-default; use
them when agent context should stay tight.

```bash
curl -sS --get "https://api.search.brave.com/res/v1/web/search" \
  -H "Accept: application/json" \
  -H "X-Subscription-Token: $BRAVE_API_KEY" \
  --data-urlencode "q=site:docs.exa.ai search API" \
  --data-urlencode "count=5" \
  --data-urlencode "result_filter=web"
```

## Useful Knobs

- `q`: required; use `--data-urlencode` rather than manual URL encoding.
- `count`: default service behavior is fine; set 3-10 to keep output compact.
- `offset`: non-default; use only for pagination.
- `freshness`: non-default; useful values include `pd`, `pw`, `pm`, `py`, or a
  date range.
- `country` / `search_lang`: default service behavior is usually fine; tweak
  only for location- or language-sensitive queries.
- `result_filter`: non-default; use to limit response sections when the default
  mixed response is too broad.

Return compact evidence: title, URL, age/date when available, snippet, and why
the result matters. Cite the original source URLs, not Brave itself.
