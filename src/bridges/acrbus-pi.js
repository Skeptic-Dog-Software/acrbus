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
// - ACRBUS_TURNS: set on every review's call. For each model call, stderr
//   says what the prompt cache served, from pi's own usage: a provider such
//   as Fireworks reports it in the response body, which pi reads, and never
//   in a header. And, once a call, the effort the provider says it applied,
//   where it echoes it, as Fireworks does in fireworks-conversation-options.
// - ACRBUS_EFFORT: the thinking level the call asked for. pi rounds a level
//   the model doesn't have to one it does, and stderr says so.
// - ACRBUS_ALLOWED_TOOLS: the tools this call may use, comma-separated, when
//   every role on the bridge is given the same tool definitions so their
//   prompts share a cached prefix. A call to any other tool is refused, and
//   says so. It fails closed: a list it can't read refuses every tool.
// - ACRBUS_DROP_PI_DOCS=1: a review's system prompt leaves out pi's pointer
//   to its own documentation, which says itself it's for questions about
//   the harness, never a review's: one less block every turn sends again.
//   Only that block goes, and only when it reads as pi wrote it; the tools,
//   their rules and everything else stay.
// - ACRBUS_RELAY_TOKENS: past this many prompt tokens in one turn that goes
//   on to call tools, the call hands off. It's told to stop and answer with
//   everything it would report so far, what it has checked and what's left;
//   its tools are refused from then on; and its answer is marked, so acrbus
//   starts a fresh session from the handoff instead of taking it for a
//   finished review. A turn that calls no tools is the answer itself, and
//   never hands off.

// The headers the key goes in: the provider's own, and the same through a
// gateway that forwards only what's prefixed for it.
const KEY_HEADERS = ["x-session-affinity", "x-bf-eh-x-session-affinity"];
// Response headers that count the prompt tokens read from the cache, for a
// provider that says it there.
const CACHED_HEADERS = ["fireworks-cached-prompt-tokens", "x-cached-prompt-tokens"];
// The response header in which Fireworks echoes the effort it applied.
const OPTIONS_HEADER = "fireworks-conversation-options";

// A count, as a whole number of tokens: 0 for anything that isn't one.
function count(v) {
  return typeof v === "number" && Number.isFinite(v) ? Math.max(0, Math.round(v)) : 0;
}

// The headers with their names in lower case, whatever pi passes.
function lowered(headers) {
  const out = {};
  if (headers && typeof headers === "object") {
    for (const [name, value] of Object.entries(headers)) out[name.toLowerCase()] = value;
  }
  return out;
}

// What the provider says it applied, from its options header: its
// reasoning_effort, and its thinking mode and budget when it says them. The
// header is JSON, or failing that read as name and value pairs. Only plain
// words go into the log.
function applied(headers) {
  const raw = headers[OPTIONS_HEADER];
  if (typeof raw !== "string" || raw.length === 0) return null;
  let parsed = null;
  try {
    parsed = JSON.parse(raw);
  } catch {
    parsed = null;
  }
  const value = (name) => {
    let v = null;
    if (parsed && typeof parsed === "object" && parsed[name] !== undefined && parsed[name] !== null) {
      v = String(parsed[name]);
    } else {
      const m = new RegExp(`${name}["']?\\s*[:=]\\s*["']?([A-Za-z0-9_.-]+)`).exec(raw);
      v = m ? m[1] : null;
    }
    if (v === null) return null;
    const plain = v.replace(/[^A-Za-z0-9_.-]/g, "").slice(0, 40);
    return plain.length > 0 ? plain : null;
  };
  const effort = value("reasoning_effort");
  if (!effort) return null;
  const parts = [`reasoning_effort ${effort}`];
  const mode = value("thinking_mode");
  if (mode) parts.push(`thinking_mode ${mode}`);
  const budget = value("thinking_budget_tokens");
  if (budget) parts.push(`thinking_budget_tokens ${budget}`);
  return parts.join(", ");
}

// pi's <docs> section, as pi writes it: what it says it's for, then its
// paths. Anything else in a <docs> block is left alone.
const PI_DOCS = /\n*<docs>\nPi documentation \(read only when the user asks about the harness itself[\s\S]*?\n<\/docs>/;

// What marks a handoff's answer, as its first line, for acrbus to read.
const HANDOFF_MARK = "ACRBUS-HANDOFF";
// What a call past its size is told.
const HANDOFF =
  "You've used most of the room one session has, so this session ends here, and a fresh one " +
  "carries on from what you hand it. Stop investigating, and don't call any more tools. " +
  "Answer now with only the JSON your instructions describe, holding everything your answer " +
  "would hold so far, in full, exactly as you'd report it, and one more field: " +
  '"handoff": {"checked": "<what you have checked, specifically: the files, functions, paths ' +
  'and claims>", "left": "<what is left to check, and where you were going next>"}.';

// One model call's figures, from pi's own usage: what the turn log says,
// and the size a relay checks, read once.
function figures(usage) {
  const fresh = count(usage.input);
  const cached = count(usage.cacheRead);
  const written = count(usage.cacheWrite);
  return { fresh, cached, written, out: count(usage.output), prompt: fresh + cached + written };
}

// An assistant message with the handoff mark ahead of its text.
function marked(message) {
  const content = Array.isArray(message.content) ? message.content : [];
  const at = content.findIndex((b) => b && b.type === "text");
  if (at >= 0 && String(content[at].text ?? "").startsWith(HANDOFF_MARK)) return message;
  const next =
    at >= 0
      ? content.map((b, i) => (i === at ? { ...b, text: `${HANDOFF_MARK}\n${b.text ?? ""}` } : b))
      : [...content, { type: "text", text: `${HANDOFF_MARK}\n` }];
  return { ...message, content: next };
}

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
  }
  const turns = process.env.ACRBUS_TURNS === "1";
  if (key || turns) {
    let response = 0;
    let said = false;
    pi.on("after_provider_response", (event) => {
      response += 1;
      const headers = lowered(event?.headers);
      // A provider that counts its cache hits in a header: kept beside
      // pi's own count, which every provider's body gives.
      const name = CACHED_HEADERS.find((n) => headers[n] !== undefined && headers[n] !== null);
      if (name) {
        const n = String(headers[name]).replace(/[^0-9]/g, "").slice(0, 12);
        console.error(`acrbus cache: response ${response}: ${n} prompt tokens read from the cache, by its ${name} header`);
      }
      if (turns && !said) {
        const what = applied(headers);
        if (what) {
          said = true;
          console.error(`acrbus effort: the provider applied ${what}`);
        }
      }
    });
  }
  const relayAt = Number.parseInt(process.env.ACRBUS_RELAY_TOKENS ?? "", 10);
  const relays = Number.isFinite(relayAt) && relayAt > 0;
  // The latest model call's prompt tokens, from the same reading the turn
  // log gives: what a relay checks its size against.
  let lastPrompt = 0;
  if (turns || relays) {
    let turn = 0;
    pi.on("message_end", (event) => {
      const message = event?.message;
      if (!message || message.role !== "assistant" || !message.usage) return undefined;
      turn += 1;
      const f = figures(message.usage);
      lastPrompt = f.prompt;
      if (turns) {
        console.error(
          `acrbus cache: turn ${turn}: ${f.cached} of ${f.prompt} prompt tokens read from the cache (${f.fresh} fresh, ${f.written} written, ${f.out} out)`,
        );
      }
      return undefined;
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
  if (process.env.ACRBUS_DROP_PI_DOCS === "1") {
    pi.on("before_agent_start", (event) => {
      const prompt = event?.systemPrompt;
      if (typeof prompt !== "string" || !PI_DOCS.test(prompt)) return undefined;
      return { systemPrompt: prompt.replace(PI_DOCS, "") };
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
  if (relays) {
    let relaying = false;
    pi.on("turn_end", (event) => {
      if (relaying) return undefined;
      // A turn that called no tools is the answer: nothing to hand off.
      if (!Array.isArray(event?.toolResults) || event.toolResults.length === 0) return undefined;
      // The turn's own model call, as the turn log read it.
      const read = lastPrompt;
      if (read < relayAt) return undefined;
      relaying = true;
      console.error(
        `acrbus relay: turn ${event.turnIndex ?? "?"} read ${read} prompt tokens, past ${relayAt}: handing off`,
      );
      return {
        entries: [
          ...(Array.isArray(event.entries) ? event.entries : []),
          { type: "custom_message", customType: "acrbus-relay", content: HANDOFF, display: false },
        ],
        continue: true,
      };
    });
    pi.on("tool_call", async () => {
      if (!relaying) return undefined;
      return {
        block: true,
        reason: "This session is out of room: write your handoff now, as the last message asks.",
      };
    });
    pi.on("message_end", (event) => {
      if (!relaying || event?.message?.role !== "assistant") return undefined;
      return { message: marked(event.message) };
    });
  }
}
