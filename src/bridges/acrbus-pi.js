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
//   in a header. The same line says how the turn ended (pi's stop reason,
//   and its error if it had one), how long it took from request to answer,
//   and where the request went: the gateway's provider and resolved model,
//   the provider's own count of prompt tokens, and the gateway's routing
//   key, as a hash, never as it is. A turn with no response, which pi
//   retries, says so. And, once a call, the effort the provider says it
//   applied, where it echoes it, as Fireworks does in
//   fireworks-conversation-options, or, when it doesn't, that it didn't, or
//   that no response's headers reached the extension at all.
// - ACRBUS_NO_RESPONSE_SECS, ACRBUS_NO_RESPONSE_TRIES: a model request
//   that gets no response for this many seconds, no headers before it or,
//   once a stream has begun, no event in it, is lost: it's aborted as timed
//   out, which pi's own retry takes up, and stderr says so. Bytes that carry
//   no event, such as a gateway's keep-alive, aren't a response. Pi's own
//   timeout of the same silence counts too. After that many in a row, the
//   call ends with exit code 75, which its bridge fails, never passes, so
//   acrbus retries or escalates it. A request that's answering, however
//   slowly, is never cut off, and only a model request is watched: one sent
//   between pi's before_provider_request and the turn's end.
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
// - ACRBUS_RELAY_GROWTH=1: with ACRBUS_RELAY_TOKENS, a fixer's relay. Its
//   size is what the prompt grew since the session's own first turn, not
//   the prompt itself, whose opening was there before it did anything; and
//   it hands off only once the session has made an edit, or has grown twice
//   its size without one. A handoff with nothing changed hands the next
//   session only more to read, so it waits for the work a fixer is for.

import { createHash } from "node:crypto";

// The headers the key goes in: the provider's own, and the same through a
// gateway that forwards only what's prefixed for it.
const KEY_HEADERS = ["x-session-affinity", "x-bf-eh-x-session-affinity"];
// Response headers that count the prompt tokens read from the cache, for a
// provider that says it there.
const CACHED_HEADERS = ["fireworks-cached-prompt-tokens", "x-cached-prompt-tokens"];
// The response header in which Fireworks echoes the effort it applied.
const OPTIONS_HEADER = "fireworks-conversation-options";
// Where a gateway says it sent a request, logged as they are: names a
// gateway and a provider give to themselves, never anything secret.
const ROUTE_PLAIN = [
  ["provider", "x-bifrost-provider"],
  ["model", "x-bifrost-resolved-model"],
  ["provider prompt", "fireworks-prompt-tokens"],
];
// The gateway's routing key: logged only as the first 8 hex of its
// sha256, enough to tell two keys apart, and nothing to reuse.
const ROUTE_KEY = "x-bifrost-routing-info-key";

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

// A header's value as a plain word or number: what's left of it after
// anything but letters, digits and `_ . / : -` goes, cut to 80. None if
// nothing's left.
function plain(v) {
  if (v === undefined || v === null) return null;
  const s = String(v).replace(/[^A-Za-z0-9_./:-]/g, "").slice(0, 80);
  return s.length > 0 ? s : null;
}

// Where a response says its request went, from its headers: "provider p,
// model m, provider prompt n, key #0a1b2c3d". None if it says nothing.
function routeOf(headers) {
  const parts = [];
  for (const [label, name] of ROUTE_PLAIN) {
    const v = plain(headers[name]);
    if (v) parts.push(`${label} ${v}`);
  }
  const key = headers[ROUTE_KEY];
  if (typeof key === "string" && key.length > 0) {
    parts.push(`key #${createHash("sha256").update(key).digest("hex").slice(0, 8)}`);
  }
  return parts.length > 0 ? parts.join(", ") : null;
}

// An error as one short line, with anything that looks like a token cut.
function oneLine(text) {
  return String(text)
    .replace(/\s+/g, " ")
    .replace(/[A-Za-z0-9_\-+/=]{32,}/g, "…")
    .trim()
    .slice(0, 200);
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
  'and claims>", "left": "<what is left to check, and where you were going next>"}, ' +
  "and anything else your instructions say a handoff holds.";

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

// Why a call ends when its requests keep getting no response: an exit code
// its bridge fails, as EX_TEMPFAIL says, so acrbus tries again.
const NO_RESPONSE_EXIT = 75;
// Marks a fetch already wrapped, so the guard wraps it once a process.
const GUARDED = Symbol.for("acrbus.no-response-guard");
// pi's HTTP client's own timeouts, as undici and the OpenAI SDK name them.
const OWN_TIMEOUT = /UND_ERR_(HEADERS|BODY)_TIMEOUT|timed? ?out|timeout/i;
// An SSE line that's an event, not a comment or a blank keep-alive.
const SSE_EVENT = /(^|[\r\n])(data|event):/;

// The no-response guard, when ACRBUS_NO_RESPONSE_SECS is set: it wraps
// fetch, which pi's providers call through, once per process.
function guard(pi, turnNow) {
  const secs = Number.parseFloat(process.env.ACRBUS_NO_RESPONSE_SECS ?? "");
  if (!Number.isFinite(secs) || secs <= 0) return;
  const parsed = Number.parseInt(process.env.ACRBUS_NO_RESPONSE_TRIES ?? "", 10);
  const tries = Number.isFinite(parsed) && parsed > 0 ? parsed : 3;
  const real = globalThis.fetch;
  if (typeof real !== "function" || real[GUARDED]) return;
  const ms = secs * 1000;
  // Between a provider request and its turn's end, a fetch is the model's.
  let inCall = false;
  // Lost requests in a row, and the request now waiting for an event.
  let misses = 0;
  let live = null;
  pi.on("before_provider_request", () => {
    inCall = true;
    return undefined;
  });
  const done = () => {
    inCall = false;
    live?.stop();
    return undefined;
  };
  pi.on("message_end", (event) => (event?.message?.role === "assistant" ? done() : undefined));
  pi.on("agent_end", done);
  pi.on("provider_stream_event", () => {
    live?.heard();
    return undefined;
  });
  const say = (line) => console.error(`acrbus cache: turn ${turnNow()}: ${line}`);
  // One more lost request: said, and past the limit, the call ends.
  const missed = (line) => {
    misses += 1;
    if (misses < tries) {
      say(line);
      return;
    }
    const last = `acrbus cache: turn ${turnNow()}: no response ${misses} times in a row: ending the call, so acrbus retries or escalates it\n`;
    const exit = () => process.exit(NO_RESPONSE_EXIT);
    setTimeout(exit, 2000).unref?.();
    process.stderr.write(last, exit);
  };
  const wrapped = async function (input, init = {}) {
    const method = String(init?.method ?? input?.method ?? "GET").toUpperCase();
    if (!inCall || method !== "POST") return real.call(this, input, init);
    const streamed = typeof init.body === "string" && /"stream"\s*:\s*true/.test(init.body);
    const outer = init.signal;
    const ctl = new AbortController();
    const at = Date.now();
    let timer = null;
    let answered = false;
    let ended = false;
    let body = null;
    const lost = new Error(`acrbus: no response in ${secs}s: request timed out`);
    const stop = () => {
      ended = true;
      clearTimeout(timer);
      if (live === watch) live = null;
    };
    const arm = () => {
      clearTimeout(timer);
      timer = setTimeout(() => {
        if (ended) return;
        stop();
        missed(`no response in ${secs}s, aborted and retried`);
        ctl.abort(lost);
        try {
          body?.error(lost);
        } catch {}
      }, ms);
    };
    const watch = {
      stop,
      heard() {
        if (ended || !streamed) return;
        answered = true;
        misses = 0;
        arm();
      },
    };
    const onOuter = () => {
      if (ended) return;
      // pi's own timeout of the same silence ends it from outside.
      if (!answered && Date.now() - at >= ms * 0.9) missed("no response, ended at pi's own timeout, retried");
      stop();
      ctl.abort(outer.reason);
    };
    if (outer?.aborted) ctl.abort(outer.reason);
    else outer?.addEventListener?.("abort", onOuter, { once: true });
    live?.stop();
    live = watch;
    arm();
    const ours = (e) => e === lost || ctl.signal.reason === lost;
    const own = (e) => !ours(e) && !outer?.aborted && OWN_TIMEOUT.test(`${e?.code ?? ""} ${e?.message ?? e} ${e?.cause?.code ?? ""}`);
    let res;
    try {
      res = await real.call(this, input, { ...init, signal: ctl.signal });
    } catch (e) {
      if (!ended && own(e)) missed(`no response, ended at its own timeout (${oneLine(e?.code ?? e?.message ?? e)}), retried`);
      stop();
      // Ours goes on as a timeout, never as an abort, which pi would take
      // for its own cancelling.
      throw ours(e) ? lost : e;
    }
    if (ended) return res;
    if (!streamed || !res.body) {
      misses = 0;
      stop();
      return res;
    }
    const reader = res.body.getReader();
    const text = new TextDecoder();
    const piped = new ReadableStream({
      start(c) {
        body = c;
      },
      async pull(c) {
        try {
          const { done: end, value } = await reader.read();
          if (end) {
            stop();
            c.close();
            return;
          }
          if (SSE_EVENT.test(text.decode(value, { stream: true }))) watch.heard();
          c.enqueue(value);
        } catch (e) {
          if (!ended && own(e)) missed(`no response, ended at its own timeout (${oneLine(e?.code ?? e?.message ?? e)}), retried`);
          stop();
          try {
            c.error(ours(e) ? lost : e);
          } catch {}
        }
      },
      cancel(why) {
        stop();
        return reader.cancel(why);
      },
    });
    const out = new Response(piped, { status: res.status, statusText: res.statusText, headers: res.headers });
    Object.defineProperty(out, "url", { value: res.url });
    return out;
  };
  wrapped[GUARDED] = true;
  globalThis.fetch = wrapped;
}

export default function acrbus(pi) {
  const key = process.env.ACRBUS_CACHE_KEY;
  const turns = process.env.ACRBUS_TURNS === "1";
  // The model calls this call has ended so far: the turn log's count, and
  // what the no-response guard names the turn in progress by.
  let turn = 0;
  guard(pi, () => turn + 1);
  // This model call's request: when it went, and where its response, if
  // one came, says it went. Reset as each request goes.
  let requestAt = null;
  let route = null;
  let responded = false;
  // Whether any response's headers reached this extension, and how many
  // the last had; whether the effort the provider applied has been said,
  // and whether, a turn having ended without it, that it wasn't.
  let anyHeaders = false;
  let lastCount = 0;
  let appliedSaid = false;
  let unsaidSaid = false;
  if (key) {
    pi.on("before_provider_headers", (event) => {
      for (const name of KEY_HEADERS) event.headers[name] = key;
    });
  }
  if (key || turns) {
    // One handler: each request is timed for the turn log, and an OpenAI
    // chat request gets the key as its `user`.
    pi.on("before_provider_request", (event, ctx) => {
      if (turns) {
        requestAt = Date.now();
        route = null;
        responded = false;
      }
      const payload = event.payload;
      if (!key || !openaiChat(payload, ctx?.model) || payload.user !== undefined) return undefined;
      return { ...payload, user: key };
    });
  }
  if (key || turns) {
    let response = 0;
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
      if (!turns) return;
      responded = true;
      lastCount = Object.keys(headers).length;
      if (lastCount > 0) anyHeaders = true;
      route = routeOf(headers);
      if (!appliedSaid) {
        const what = applied(headers);
        if (what) {
          appliedSaid = true;
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
  // The last turn had no response: said again at the call's end if no
  // turn followed it.
  let unanswered = false;
  if (turns || relays) {
    pi.on("message_end", (event) => {
      const message = event?.message;
      if (!message || message.role !== "assistant" || !message.usage) return undefined;
      turn += 1;
      const f = figures(message.usage);
      lastPrompt = f.prompt;
      if (turns) {
        const how = [];
        if (message.stopReason) how.push(`stop ${plain(message.stopReason) ?? "?"}`);
        // The tool calls it asked for at once, by name: what shows batching.
        const calls = Array.isArray(message.content)
          ? message.content.filter((c) => c?.type === "toolCall")
          : [];
        const byName = new Map();
        for (const c of calls) {
          const name = plain(c.name) ?? "?";
          byName.set(name, (byName.get(name) ?? 0) + 1);
        }
        how.push(
          calls.length === 0
            ? "tools 0"
            : `tools ${calls.length} (${[...byName].map(([n, k]) => `${n} ${k}`).join(", ")})`,
        );
        if (requestAt !== null) how.push(`${((Date.now() - requestAt) / 1000).toFixed(1)}s`);
        if (message.errorMessage) how.push(`error: ${oneLine(message.errorMessage)}`);
        unanswered = f.prompt === 0 && f.out === 0;
        if (unanswered) how.push("no response, retried");
        how.push(
          responded
            ? `route: ${route ?? "the response named none"}`
            : "route: no response headers",
        );
        console.error(
          `acrbus cache: turn ${turn}: ${f.cached} of ${f.prompt} prompt tokens read from the cache (${f.fresh} fresh, ${f.written} written, ${f.out} out); ${how.join(", ")}`,
        );
        if (!appliedSaid && !unsaidSaid) {
          unsaidSaid = true;
          console.error(
            anyHeaders
              ? `acrbus effort: the provider didn't say what effort it applied: no ${OPTIONS_HEADER} among its ${lastCount} response headers`
              : "acrbus effort: no response's headers reached acrbus's extension, so the effort the provider applied is unknown",
          );
        }
        requestAt = null;
      }
      return undefined;
    });
    if (turns) {
      pi.on("agent_end", () => {
        if (unanswered) {
          console.error("acrbus cache: the call ended on a turn with no response");
        }
        return undefined;
      });
    }
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
    const growth = process.env.ACRBUS_RELAY_GROWTH === "1";
    let relaying = false;
    // The session's first turn's prompt, which a fixer's growth counts
    // from, and the edits it has asked for.
    let firstPrompt = null;
    let edits = 0;
    pi.on("turn_end", (event) => {
      if (relaying) return undefined;
      // The turn's own model call, as the turn log read it.
      const read = lastPrompt;
      // A turn with no response read nothing: the first that did is first.
      if (firstPrompt === null && read > 0) firstPrompt = read;
      // A turn that called no tools is the answer: nothing to hand off.
      if (!Array.isArray(event?.toolResults) || event.toolResults.length === 0) return undefined;
      let why;
      if (growth) {
        const grew = read - (firstPrompt ?? read);
        if (grew < relayAt || (edits === 0 && grew < 2 * relayAt)) return undefined;
        why =
          `grew ${grew} prompt tokens since its first turn's ${firstPrompt}, ` +
          (edits > 0 ? `past ${relayAt}, after an edit` : `past twice ${relayAt}, with no edit`);
      } else {
        if (read < relayAt) return undefined;
        why = `read ${read} prompt tokens, past ${relayAt}`;
      }
      relaying = true;
      console.error(`acrbus relay: turn ${event.turnIndex ?? "?"} ${why}: handing off`);
      return {
        entries: [
          ...(Array.isArray(event.entries) ? event.entries : []),
          { type: "custom_message", customType: "acrbus-relay", content: HANDOFF, display: false },
        ],
        continue: true,
      };
    });
    pi.on("tool_call", async (event) => {
      if (!relaying) {
        if (event?.toolName === "edit" || event?.toolName === "write") edits += 1;
        return undefined;
      }
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
