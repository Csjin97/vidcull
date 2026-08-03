import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  EXPECTED_PROTOCOL_VERSION,
  gateCommand,
  interpretPong,
  statusClass,
  statusLabel,
} from "./daemon";

describe("interpretPong", () => {
  it("treats the expected protocol version as compatible online", () => {
    const result = interpretPong(EXPECTED_PROTOCOL_VERSION);
    expect(result).toEqual({
      ok: true,
      protocolVersion: EXPECTED_PROTOCOL_VERSION,
      compatible: true,
    });
    expect(statusLabel(result)).toBe(`daemon: online (v${EXPECTED_PROTOCOL_VERSION})`);
    expect(statusClass(result)).toBe("status--ok");
  });

  it("flags a newer daemon and guides the user to update (§M3)", () => {
    const result = interpretPong(EXPECTED_PROTOCOL_VERSION + 1);
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.compatible).toBe(false);
      expect(result.protocolVersion).toBe(EXPECTED_PROTOCOL_VERSION + 1);
    }
    expect(statusClass(result)).toBe("status--bad");
    const label = statusLabel(result);
    expect(label).toContain("버전 불일치");
    expect(label).toContain("업데이트");
  });

  it("renders an offline label and bad class for a failed ping", () => {
    const result = { ok: false, error: "connection refused" } as const;
    expect(statusLabel(result)).toBe("daemon: offline");
    expect(statusClass(result)).toBe("status--bad");
  });
});

describe("gateCommand — the protocol-version hard gate", () => {
  const compatible = interpretPong(EXPECTED_PROTOCOL_VERSION);
  const mismatched = interpretPong(EXPECTED_PROTOCOL_VERSION + 1);
  const offline = { ok: false, error: "connection refused" } as const;
  const unknown = { ok: false, error: "연결 확인 전" } as const;

  it("stays open for a compatible daemon", () => {
    expect(gateCommand("list_groups", compatible)).toBeNull();
    expect(gateCommand("delete_files", compatible)).toBeNull();
  });

  it("stays open while the daemon is offline or not yet pinged", () => {
    expect(gateCommand("list_groups", offline)).toBeNull();
    expect(gateCommand("list_groups", unknown)).toBeNull();
  });

  it("blocks every data RPC on a version-mismatched daemon", () => {
    for (const cmd of [
      "list_groups",
      "list_group_detail",
      "group_stats",
      "daemon_progress",
      "delete_files",
      "get_settings",
      "set_settings",
      "rescan_directory",
      "force_rescan_directory",
    ]) {
      const msg = gateCommand(cmd, mismatched);
      expect(msg, `${cmd} must be blocked`).not.toBeNull();
      expect(msg).toContain(`v${EXPECTED_PROTOCOL_VERSION + 1}`);
      expect(msg).toContain(`v${EXPECTED_PROTOCOL_VERSION}`);
      expect(msg).toContain("업데이트");
    }
  });

  it("exempts the ping handshake and the local OS commands", () => {
    expect(gateCommand("ping_daemon", mismatched)).toBeNull();
    expect(gateCommand("reveal_in_folder", mismatched)).toBeNull();
    expect(gateCommand("pick_folder", mismatched)).toBeNull();
  });
});

describe("EXPECTED_PROTOCOL_VERSION stays in sync with the Rust daemon (§M3)", () => {
  it("equals vidcull_ipc::PROTOCOL_VERSION", () => {
    const protocolRs = resolve(process.cwd(), "../crates/vidcull-ipc/src/protocol.rs");
    const source = readFileSync(protocolRs, "utf8");
    const match = source.match(/PROTOCOL_VERSION\s*:\s*u32\s*=\s*(\d+)/);
    expect(match, "PROTOCOL_VERSION constant found in protocol.rs").not.toBeNull();
    const rustVersion = Number(match![1]);
    expect(EXPECTED_PROTOCOL_VERSION).toBe(rustVersion);
  });
});
