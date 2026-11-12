// acrbus's extension for pi, loaded into a review's calls only. It does two
// things, each only when its bridge sets the variable for it:
//
// - ACRBUS_CACHE_KEY: sends it as x-session-affinity, so the calls of a round
//   that share a prefix reach the same replica's prompt cache, as Fireworks
//   asks of a caller that wants one call's cache to serve another's.
// - ACRBUS_ALLOWED_TOOLS: the tools this call may use, comma-separated, when
//   every role on the bridge is given the same tool definitions so their
//   prompts share a cached prefix. A call to any other tool is refused, and
//   says so. It fails closed: a list it can't read refuses every tool.
export default function acrbus(pi) {
  const key = process.env.ACRBUS_CACHE_KEY;
  if (key) {
    pi.on("before_provider_headers", (event) => {
      event.headers["x-session-affinity"] = key;
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
