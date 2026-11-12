// acrbus's extension for pi, loaded into a review's calls only. What it does
// depends on the variables its bridge sets:
//
// - ACRBUS_CACHE_KEY: the key that sends the calls of a round that share a
//   prefix to the same replica's prompt cache. Fireworks keeps a cache per
//   replica and takes the key as the x-session-affinity header or as the
//   request's `user`. A gateway in between may forward neither as sent:
//   Bifrost passes a header on to the provider only when it's prefixed
//   x-bf-eh-, which it strips. So the key goes three ways: as
//   x-session-affinity, as x-bf-eh-x-session-affinity, and as `user` in an
//   OpenAI chat request that doesn't set its own. Each response's cached
//   prompt tokens, where its provider says in a header, go to stderr, so a
//   run's log shows its hits.
// - ACRBUS_EFFORT: the thinking level the call asked for. pi rounds a level
//   the model doesn't have to one it does, and stderr says so.
// - ACRBUS_ALLOWED_TOOLS: the tools this call may use, comma-separated, when
//   every role on the bridge is given the same tool definitions so their
//   prompts share a cached prefix. A call to any other tool is refused, and
//   says so. It fails closed: a list it can't read refuses every tool.

// The headers the key goes in: the provider's own, and the same through a
// gateway that forwards only what's prefixed for it.
const KEY_HEADERS = ["x-session-affinity", "x-bf-eh-x-session-affinity"];
// Response headers that count the prompt tokens read from the cache.
const CACHED_HEADERS = ["fireworks-cached-prompt-tokens", "x-cached-prompt-tokens"];

// An OpenAI chat completions request, which takes `user`: by the model's API
// when pi says, otherwise by its shape, so no other API's request gets a
// field it would refuse.
function openaiChat(payload, model) {
  if (!payload || typeof payload !== "object" || Array.isArray(payload)) return false;
  if (model && typeof model.api === "string") return model.api === "openai-completions";
  return (
    Array.isArray(payload.messages) &&
    !("system" in payload) &&
    !("contents" in payload) &&
    !("input" in payload)
  );
}

export default function acrbus(pi) {
  const key = process.env.ACRBUS_CACHE_KEY;
  if (key) {
    pi.on("before_provider_headers", (event) => {
      for (const name of KEY_HEADERS) event.headers[name] = key;
    });
    pi.on("before_provider_request", (event, ctx) => {
      const payload = event.payload;
      if (!openaiChat(payload, ctx?.model) || payload.user !== undefined) return undefined;
      return { ...payload, user: key };
    });
    let turn = 0;
    pi.on("after_provider_response", (event) => {
      turn += 1;
      const headers = event?.headers ?? {};
      const name = CACHED_HEADERS.find((n) => headers[n] !== undefined && headers[n] !== null);
      if (name) {
        console.error(`acrbus cache: turn ${turn}: ${headers[name]} prompt tokens read from the cache (${name})`);
      }
    });
  }
  const effort = process.env.ACRBUS_EFFORT;
  if (effort) {
    let said = false;
    pi.on("before_agent_start", (event, ctx) => {
      const used = ctx?.thinkingLevel;
      if (said || !used || used === effort) return undefined;
      said = true;
      const model = ctx?.model?.id ?? "this model";
      console.error(`acrbus effort: asked for thinking ${effort}, which ${model} doesn't have, so pi uses ${used}`);
      return undefined;
    });
  }
  const listed = process.env.ACRBUS_ALLOWED_TOOLS;
  if (listed !== undefined) {
    const allowed = /^[A-Za-z0-9_.,:-]*$/.test(listed)
      ? new Set(listed.split(",").filter((t) => t.length > 0))
      : new Set();
    pi.on("tool_call", async (event) => {
      if (allowed.has(event.toolName)) return undefined;
      const may = allowed.size > 0 ? [...allowed].join(", ") : "none";
      return {
        block: true,
        reason: `This role may not use ${event.toolName}. The tools it may use: ${may}.`,
      };
    });
  }
}
