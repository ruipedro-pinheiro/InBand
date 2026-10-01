import { describe, expect, test } from "bun:test";
import { Bridge, type BridgeConfig } from "../src/bridge.ts";
import { testDb } from "./helpers.ts";

const CONFIG: BridgeConfig = { port: 0, maxMessageBytes: 64 * 1024, wake: {} };

describe("channel mailbox subscription", () => {
  test("reports a push to a connected channel instead of a missing wake", async () => {
    const bridge = new Bridge(testDb(), CONFIG);
    const pending = bridge.subscribeMailbox("claude-web-c3d4", 5, undefined, 0);

    const sent = bridge.send("claude-api-a1b2", "claude-web-c3d4", "run the tests");
    expect(sent.notify).toEqual({ "claude-web-c3d4": "pushed-to-channel" });
    await expect(pending).resolves.toMatchObject([{ recipient: "claude-web-c3d4", content: "run the tests" }]);

    expect(bridge.send("claude-api-a1b2", "claude-web-c3d4", "no channel now").notify).toEqual({
      "claude-web-c3d4": "no-wake-configured",
    });
  });

  test("replays unread mail queued before the shim connects without consuming it", async () => {
    const bridge = new Bridge(testDb(), CONFIG);
    bridge.send("opencode", "claude-desktop-a1b2", "queued before startup");

    await expect(bridge.subscribeMailbox("claude-desktop-a1b2", 1, undefined, 0)).resolves.toMatchObject([
      { recipient: "claude-desktop-a1b2", content: "queued before startup" },
    ]);
    expect(bridge.fetchUnread("claude-desktop-a1b2")).toHaveLength(1);
  });

  test("reconnect cursor replays only later unread deliveries from the same family", async () => {
    const bridge = new Bridge(testDb(), CONFIG);
    const first = bridge.send("opencode", "claude-desktop-a1b2", "already notified");
    bridge.send("opencode", "other-desktop-a1b2", "another family");
    bridge.send("opencode", "claude-desktop-b2c3", "already consumed");
    bridge.fetchUnread("claude-desktop-b2c3");
    bridge.send("opencode", "claude-desktop-a1b2", "queued during reconnect");

    await expect(bridge.subscribeFamily("claude", 1, undefined, first.messageId)).resolves.toMatchObject([
      { recipient: "claude-desktop-a1b2", content: "queued during reconnect" },
    ]);
    expect(bridge.fetchUnread("claude-desktop-a1b2")).toHaveLength(2);
  });

  test("replayed broadcasts retain their concrete delivery targets", async () => {
    const bridge = new Bridge(testDb(), CONFIG);
    bridge.touchAgent("claude-desktop-a1b2");
    bridge.touchAgent("claude-desktop-b2c3");
    bridge.send("opencode", "all", "queued broadcast");
    bridge.fetchUnread("claude-desktop-b2c3");

    await expect(bridge.subscribeFamily("claude", 1, undefined, 0)).resolves.toMatchObject([
      { recipient: "claude-desktop-a1b2", content: "queued broadcast" },
    ]);
  });

  test("cursor skips old previews and still receives new live mail", async () => {
    const bridge = new Bridge(testDb(), CONFIG);
    const first = bridge.send("opencode", "claude-desktop-a1b2", "already notified");
    const next = bridge.subscribeMailbox("claude-desktop-a1b2", 1, undefined, first.messageId);
    bridge.send("opencode", "claude-desktop-a1b2", "new mail");

    await expect(next).resolves.toMatchObject([{ content: "new mail" }]);
  });

  test("legacy subscribers without a cursor do not repeatedly replay unread mail", async () => {
    const bridge = new Bridge(testDb(), CONFIG);
    bridge.send("opencode", "claude-desktop-a1b2", "old mail");
    const next = bridge.subscribeMailbox("claude-desktop-a1b2", 1);
    bridge.send("opencode", "claude-desktop-a1b2", "new mail");
    await expect(next).resolves.toMatchObject([{ content: "new mail" }]);
  });

  test("resolves only messages sent to the exact mailbox", async () => {
    const bridge = new Bridge(testDb(), CONFIG);
    const subscription = bridge.subscribeMailbox("claude-desktop-a1b2", 5);

    bridge.send("opencode", "claude-desktop-a1b2-child", "not yours");
    bridge.send("opencode", "claude-desktop-a1b2", "yours");

    await expect(subscription).resolves.toMatchObject([
      { sender: "opencode", recipient: "claude-desktop-a1b2", content: "yours" },
    ]);
  });

  test("registers the exact mailbox so channel-only sessions receive later broadcasts", async () => {
    const bridge = new Bridge(testDb(), CONFIG);
    const subscription = bridge.subscribeMailbox("claude-desktop-a1b2", 5);

    const result = bridge.send("opencode", "all", "broadcast");

    expect(result.deliveredTo).toEqual(["claude-desktop-a1b2"]);
    await expect(subscription).resolves.toMatchObject([
      { sender: "opencode", recipient: "claude-desktop-a1b2", content: "broadcast" },
    ]);
    expect(bridge.fetchUnread("claude-desktop-a1b2")).toMatchObject([
      { sender: "opencode", recipient: "all", content: "broadcast" },
    ]);
  });

  test("caps pending channel subscriptions per target", async () => {
    const bridge = new Bridge(testDb(), CONFIG);
    const cleanups: Array<() => void> = [];
    const subscriptions = Array.from({ length: 8 }, () =>
      bridge.subscribeFamily("claude", 300, (cleanup) => cleanups.push(cleanup)),
    );

    await expect(bridge.subscribeFamily("claude", 300)).rejects.toThrow(/too many pending subscriptions/i);
    for (const cleanup of cleanups) cleanup();
    await expect(Promise.all(subscriptions)).resolves.toEqual(Array.from({ length: 8 }, () => []));
  });
});
