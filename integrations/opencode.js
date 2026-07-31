/**
 * agentbus reporter for OpenCode.
 *
 * OpenCode writes no transcript this can read, and never publishes its state
 * into the terminal title — it stays "OpenCode" whether thinking or idle — so
 * there is nothing to observe passively. This plugin reports it as fact
 * instead, which is also the only way `blocked` is ever known for OpenCode.
 *
 * Install with `just sync-opencode`. OpenCode loads anything in
 * ~/.config/opencode/plugin/ automatically; no opencode.json change needed.
 */

const AGENTBUS = process.env.AGENTBUS_BIN || `${process.env.HOME}/.local/bin/agentbus`;

export const AgentbusReporter = async ({ $ }) => {
  // Only report transitions. Events like message.part.updated fire constantly,
  // and each report is a process spawn.
  let last = null;

  const report = (state, detail = "") => {
    if (state === last) return;
    last = state;
    // No check for which multiplexer this is: agentbus works that out from the
    // environment and declines to report when there is no pane to attribute to.
    // Testing for one here would silently exclude the others.
    try {
      // Deliberately not awaited, and stdin explicitly closed.
      //
      // A spawned command inherits this process's stdin, which is the TUI's
      // terminal. Anything that reads stdin then never sees EOF: it hangs, and
      // it eats the keystrokes meant for OpenCode. Awaiting it stalls the event
      // loop on top of that — the agent answers one prompt and then ignores
      // every key. Closing stdin fixes the cause; not awaiting means even a
      // command that hangs anyway cannot take OpenCode down with it.
      $`${AGENTBUS} hook state ${state} ${detail} < /dev/null`.quiet().nothrow();
    } catch {
      // A monitoring reporter must never be able to disrupt the agent.
    }
  };

  return {
    event: ({ event }) => {
      switch (event?.type) {
        case "permission.asked":
        case "question.asked":
          report("blocked", "permission");
          break;
        case "permission.replied":
        case "question.replied":
          report("working");
          break;
        case "session.idle":
          report("done");
          break;
        case "message.updated":
        case "message.part.updated":
        case "message.part.delta":
          report("working");
          break;
      }
    },
  };
};
