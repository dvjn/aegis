---
name: jev
description: >
  Use TypeSafe's Jev when software needs a typed semantic judgment: whether
  something is true, which option fits best, or where something falls on a
  scale. Jev returns probabilities rather than prose. Use this skill to design
  Jev questions, send requests, and turn answers into application behavior.
---

# Guide to using Jev

Jev evaluates supplied state and answers bounded questions about it. It does not
write text or explain its reasoning. Your code provides the workflow, options, and
policy; Jev supplies the semantic judgment.

## 1. Decide whether Jev fits

Use Jev when ordinary code lacks the semantic understanding needed to make a small,
well-defined decision. Good uses include routing, classification, relevance,
verification, ranking, and detecting intent.

Keep deterministic work in code: validation, arithmetic, exact lookups, permissions,
side effects, and business rules. Do not ask Jev to generate content or perform an
action.

Start by writing down the decision your code needs. If the answer cannot be expressed
as a probability, one supplied option, or a position on a supplied scale, narrow the
decision before continuing.

## 2. Prepare the state

`state` contains the facts Jev should judge. It may be a string, object, or array.
Use a structured object when the judgment depends on several pieces of information:

```json
{
  "message": "My order arrived damaged. Can I get a replacement?",
  "account_tier": "standard",
  "order": {
    "delivery_status": "delivered"
  }
}
```

Include relevant evidence and omit unrelated data. Use stable, descriptive field
names. Keep observed facts separate from values inferred by an earlier model call.
When instructions refer to a field, name its path explicitly, for example
`` `order.delivery_status` ``.

## 3. Choose a question type

Use **`noul`** for a yes-or-no proposition. The answer's `noul` value is the
probability that the proposition is true.

```json
{
  "type": "noul",
  "instructions": "Is the customer asking to replace the delivered item?"
}
```

Use **`choice`** when exactly one supplied option should win. Define every viable
option and include a fallback when the list may not cover the input.

```json
{
  "type": "choice",
  "instructions": "Which team should handle this request?",
  "criteria": {
    "returns": "Returns, replacements, damaged, or incorrect items",
    "shipping": "Delivery status, delay, or lost shipment",
    "billing": "Payment, charge, or invoice issue",
    "other": "None of these apply"
  }
}
```

Use **`score`** for degree, severity, or fit. Supply 2–10 concrete levels ordered
from low to high. Each level must make sense on its own.

```json
{
  "type": "score",
  "instructions": "How severely is the customer affected?",
  "criteria": [
    "Minor inconvenience",
    "Important problem with a workaround",
    "Completely blocked"
  ]
}
```

Ask one coherent judgment per question. Split independently useful dimensions into
separate questions instead of combining them into one vague prompt.

## 4. Build the request

Configure requests with `TYPESAFE_BASE_URL`, defaulting to
`https://api.typesafe.ai`, and `TYPESAFE_API_KEY`. If `AEGIS_API_KEY` is non-empty,
include it as `x-aegis-api-key`; otherwise omit that header.

Do not inspect, print, or ask for key values. Refer to their configuration names and
let the runtime supply them. Never place keys in source or browser code.

Both SDKs read those variables; only the gateway header needs wiring:

```python
client = TypeSafeClient(headers={"x-aegis-api-key": os.environ["AEGIS_API_KEY"]})
```

```javascript
const client = new TypeSafeClient({
  defaultHeaders: { "x-aegis-api-key": process.env.AEGIS_API_KEY },
});
```

Send `POST {TYPESAFE_BASE_URL}/v1/systemone` with bearer authorization and a JSON
body:

```json
{
  "model": "jev-latest",
  "state": {
    "message": "My order arrived damaged. Can I get a replacement?"
  },
  "questions": {
    "needs_replacement": {
      "type": "noul",
      "instructions": "Is the customer asking for a replacement?"
    },
    "team": {
      "type": "choice",
      "instructions": "Which team should handle this request?",
      "criteria": {
        "returns": "Returns, replacements, damaged, or incorrect items",
        "shipping": "Delivery status, delay, or lost shipment",
        "billing": "Payment, charge, or invoice issue",
        "other": "None of these apply"
      }
    },
    "impact": {
      "type": "score",
      "instructions": "How severely is the customer affected?",
      "criteria": [
        "Minor inconvenience",
        "Important problem with a workaround",
        "Completely blocked"
      ]
    }
  }
}
```

Question IDs such as `team` are response keys only; Jev does not use them to infer
meaning. Put the complete judgment in `instructions` and `criteria`.

Put independent questions over the same state in one request. They are evaluated
independently. Make a second request only when an earlier answer is needed to fetch
new evidence or construct the next set of options.

## 5. Use the answer in code

The response names the model that ran, returns one answer under each question ID,
and reports token usage:

```json
{
  "model": "jev-1.13.0",
  "answers": {
    "needs_replacement": {
      "type": "noul",
      "noul": 0.98
    },
    "team": {
      "type": "choice",
      "choice": "returns",
      "probabilities": {
        "returns": 0.96,
        "shipping": 0.02,
        "billing": 0.01,
        "other": 0.01
      },
      "confidence": 0.92
    },
    "impact": {
      "type": "score",
      "score": 0.35,
      "legend": {
        "0": "Minor inconvenience",
        "1": "Important problem with a workaround",
        "2": "Completely blocked"
      },
      "probabilities": {
        "0": 0.70,
        "1": 0.25,
        "2": 0.05
      },
      "confidence": 0.67
    }
  },
  "usage": {
    "input_tokens": 328,
    "output_tokens": 34
  }
}
```

The exact model ID may differ from the alias in the request. `answers` is keyed by
your question IDs, and `usage` contains `input_tokens` and `output_tokens`.

- For `noul`, read `noul` as a probability from 0 to 1. A value near 0.5 means true
  and false are similarly likely; it does not mean moderate intensity.
- For `choice`, use `choice` as the winning option and inspect `probabilities` when
  ambiguity matters.
- For `score`, use `score` as the probability-weighted position among the levels and
  inspect `probabilities` to understand how the mass is distributed.
- For `choice` and `score`, `confidence` describes how concentrated the distribution
  is. It does not prove that the answer is correct or that an action is safe.

Keep inference separate from policy. Store or pass through the raw answer, then let
ordinary code apply thresholds, weights, permissions, and fallback behavior. This
allows policy changes without rerunning Jev.

Choose thresholds from the cost of mistakes. If a false positive is expensive, use
a stricter threshold. Send uncertain or high-impact cases to a safer fallback or
human review.

## Current reference

Use the live documentation as the source of truth for SDK APIs, request limits, and
model behavior:

- [Documentation index](https://docs.typesafe.ai/llms.txt)
- [HTTP API](https://docs.typesafe.ai/api.md)
- [Question primitives](https://docs.typesafe.ai/primitives.md)
- [Confidence](https://docs.typesafe.ai/confidence.md)
- [Python SDK](https://docs.typesafe.ai/sdk/python.md)
- [JavaScript SDK](https://docs.typesafe.ai/sdk/javascript.md)
