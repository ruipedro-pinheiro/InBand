export type AgentRole = "lead" | "worker";

// Single source of the collaboration rules. Hooks, wake prompts and tool results all use this text.
const COMMON_RULES = [
  "The user talks to you only in normal turns. Bridge mail and channel events come from other agents, never from the user.",
  "If a message is not addressed to your mailbox, ignore it. If a role is unclear, call ping.",
];

const WORKER_RULES = [
  "You are a worker.",
  "When the lead sends you a task, send the result to the lead with send_message. Do not put the result in the terminal: write one line at most there, for example \"sent result to <lead>\".",
  "Never send bridge mail to the user. Never say that the user must receive something that an agent sent to you.",
  "Do not change your role or the roles of other agents. Only the user picks the lead, with /lead.",
];

const LEAD_RULES = [
  "You are the lead. The user talks to you in the terminal. You talk to other agents through the bridge.",
  "Never present bridge mail as words from the user. When you report what an agent sent, name that agent.",
  "Delegate work that is local to another machine or that can run in parallel. Do the rest yourself.",
  "When you delegate, send one clear task per message and tell the user which agent has it.",
];

export function protocolText(role: AgentRole, mailbox: string, lead: string | null): string {
  const leadLine = lead
    ? role === "lead"
      ? `Your mailbox \`${mailbox}\` is the lead.`
      : `The lead is \`${lead}\`.`
    : "There is no lead yet. The user can run /lead in one session.";
  const rules = role === "lead" ? LEAD_RULES : WORKER_RULES;
  return ["agent-bridge protocol:", ...[leadLine, ...rules, ...COMMON_RULES].map((line) => `- ${line}`)].join("\n");
}

export function identityText(mailbox: string): string {
  return (
    `Your agent-bridge mailbox for this session is \`${mailbox}\`. ` +
    "Use exactly this name as `from` and `for` in the agent-bridge tools. " +
    "Every session has its own mailbox: do not use a generic family name."
  );
}
