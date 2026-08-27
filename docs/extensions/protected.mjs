#!/usr/bin/env node
/** protected — the tool_call hook as a guard over sensitive paths: denies
 *  any tool call (read, write, edit, grep, bash) that touches a
 *  credential-shaped path — `~/.ssh`, `~/.aws`, `~/.gnupg`, `.env*`,
 *  `*.pem`, `*.key`. Complements gate.mjs's destructive-command denylist:
 *  this one is about what gets *read into context* or *written to disk*,
 *  not just what bash runs. Fail-open like gate.mjs: only an explicit
 *  block stops a call.
 *
 * Copy scaffold.mjs + protected.mjs into ~/.e/extensions/ (chmod +x) and
 * restart e, then ask the model to read ~/.ssh/id_rsa and watch it be
 * refused.
 */

import { connect } from "./scaffold.mjs";

// Boundaries are deliberately loose: `path` args are clean paths (`$` ends
// the string), but `command` is free-form bash text where the same segment
// is followed by a space, quote, or shell operator instead. Both cases match
// the same patterns via `(\/|$|[^\w.-])` after the segment.
const DENIED = [
  /(^|[/\s])\.ssh(\/|$|[^\w.-])/,
  /(^|[/\s])\.aws(\/|$|[^\w.-])/,
  /(^|[/\s])\.gnupg(\/|$|[^\w.-])/,
  /(^|[/\s])\.env(\.[\w-]*)?(\/|$|[^\w.-])/,
  /\.pem(\/|$|[^\w.-])/,
  /\.key(\/|$|[^\w.-])/,
];

function matches(text) {
  return typeof text === "string" && DENIED.some((pattern) => pattern.test(text));
}

connect({
  manifest: {
    name: "protected",
    version: "1.0",
    description: "blocks tool calls that touch credential-shaped paths (example)",
    hooks: ["tool_call"],
  },
  hookToolCall({ name, arguments: args }) {
    const path = args && typeof args === "object" ? args.path : undefined;
    if (matches(path)) {
      return { block: true, reason: `denied by protected: sensitive path ${path}` };
    }
    if (name === "bash" && matches(args?.command)) {
      return { block: true, reason: "denied by protected: sensitive path in command" };
    }
    return { block: false };
  },
}).run();
